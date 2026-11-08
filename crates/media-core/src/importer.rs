//! Import from a camera card (SD) into the backup drive, without scanning the backup drive.
//!
//! 1. The card is scanned as a temporary source (small: seconds). Copies already indexed anywhere
//!    are recognised by size + time + 64 KiB fingerprint, so nothing on the backup drive is read
//!    except the fingerprint of a same-size file, once.
//! 2. New files = files on the card with no copy on another drive (`copies: "only_here"`).
//! 3. They are copied with the organizer (copy mode, chosen folder structure); each copy is
//!    indexed as it is written (`journal::copy_indexed`) and the batch can be undone.
//! 4. The card is forgotten again (its files stay on the card), so the library does not show the
//!    card's copies.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::{self, FileFilter};
use crate::organizer::{self, Mode, OrganizeOptions, Rename};
use crate::scanner::{self, ScanOptions};
use crate::{identity, volumes, Db, ProgressFn, StopFlag};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportSettings {
    /// Folder on the backup drive the photos go into (e.g. `E:\사진`).
    pub target_dir: String,
    /// Folder structure below it (organizer tokens).
    pub template: String,
}

impl Default for ImportSettings {
    fn default() -> Self {
        Self { target_dir: String::new(), template: "{year}/{year}-{month}".into() }
    }
}

impl ImportSettings {
    pub fn load(conn: &Connection) -> Result<Self> {
        let v: Option<String> = conn.query_row("SELECT value FROM settings WHERE key = 'import'", [], |r| r.get(0)).optional()?;
        Ok(v.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
    }
    pub fn save(&self, conn: &Connection) -> Result<()> {
        conn.execute(
            "INSERT INTO settings(key, value) VALUES ('import', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [serde_json::to_string(self)?],
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Card {
    pub mount: String,
    pub label: String,
}

/// Camera cards plugged in right now: removable drives with a `DCIM` folder. (`MO_CARDS`, a list
/// of folders separated by `;`, adds cards for testing on systems without drive letters.)
pub fn find_cards() -> Vec<Card> {
    let mut out: Vec<Card> = volumes::mounted()
        .into_values()
        .filter(|v| v.removable && v.root.join("DCIM").is_dir())
        .map(|v| Card { mount: v.root.to_string_lossy().into_owned(), label: v.label })
        .collect();
    if let Ok(extra) = std::env::var("MO_CARDS") {
        for p in extra.split(';').filter(|p| !p.is_empty()) {
            let root = PathBuf::from(p);
            if root.join("DCIM").is_dir() {
                let label = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string());
                out.push(Card { mount: p.to_string(), label });
            }
        }
    }
    out.sort_by(|a, b| a.mount.cmp(&b.mount));
    out
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Preview {
    pub source_id: i64,
    pub volume_id: i64,
    /// On the card and not anywhere else yet.
    pub new_files: i64,
    pub new_bytes: i64,
    /// On the card and already copied somewhere (skipped).
    pub already: i64,
}

/// What would be imported: new files are the card's files with no copy on another drive.
fn new_on_card(volume_id: i64) -> FileFilter {
    FileFilter { volume_id: Some(volume_id), copies: Some("only_here".into()), ..Default::default() }
}

/// Scan the card (registered as a temporary source) and count what is new.
pub fn preview(db: &Db, card: &Path, opts: &ScanOptions, stop: &StopFlag, progress: ProgressFn) -> Result<Preview> {
    let source_id = {
        let conn = db.conn()?;
        volumes::add_source(&conn, card)?
    };
    scanner::scan_source(db, source_id, opts, stop, progress)?;
    let conn = db.conn()?;
    // Same-size files elsewhere get their fingerprint (once) so copies are recognised by content.
    identity::backfill_prehashes(&conn)?;
    let volume_id: i64 = conn.query_row("SELECT volume_id FROM sources WHERE id = ?", [source_id], |r| r.get(0))?;
    let all = db::filtered_stats(&conn, &FileFilter { volume_id: Some(volume_id), ..Default::default() })?;
    let new = db::filtered_stats(&conn, &new_on_card(volume_id))?;
    Ok(Preview { source_id, volume_id, new_files: new.count, new_bytes: new.bytes, already: all.count - new.count })
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ImportReport {
    pub batch_id: String,
    pub copied: u64,
    pub bytes: i64,
    pub already: i64,
    pub failed: u64,
    pub errors: Vec<String>,
    pub target_dir: String,
}

/// Copy the card's new files into the backup folder (indexed as they are written), then forget
/// the card.
pub fn run(db: &Db, card: &Path, settings: &ImportSettings, opts: &ScanOptions, stop: &StopFlag, progress: ProgressFn) -> Result<ImportReport> {
    if settings.target_dir.trim().is_empty() {
        return Err(anyhow!("가져올 위치(백업 폴더)를 먼저 고르세요"));
    }
    let pv = preview(db, card, opts, stop, progress)?;
    let mut rep = ImportReport { already: pv.already, target_dir: settings.target_dir.clone(), ..Default::default() };
    if pv.new_files > 0 {
        let options = OrganizeOptions {
            target_dir: settings.target_dir.clone(),
            template: settings.template.clone(),
            rename: Rename::Keep,
            mode: Mode::Copy,
            filter: new_on_card(pv.volume_id),
            uncertain_folder: Some("_날짜확인필요".into()),
            remove_empty_dirs: false,
        };
        let plan = organizer::plan(db, &options, stop, progress)?;
        let ex = organizer::execute(db, &plan, stop, progress)?;
        // Named for the history: what came from where.
        let label = card.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| card.to_string_lossy().into_owned());
        db.conn()?.execute(
            "UPDATE batches SET description = ? WHERE id = ?",
            params![format!("SD 카드 ‘{label}’에서 {}개 가져오기", ex.done), ex.batch_id],
        )?;
        rep.batch_id = ex.batch_id;
        rep.copied = ex.done;
        rep.bytes = ex.bytes;
        rep.failed = ex.failed;
        rep.errors = ex.errors;
    }
    forget(&db.conn()?, pv.source_id)?;
    Ok(rep)
}

/// Stop tracking the card: its rows go (its files stay on the card). Copies made from it keep
/// their own rows; they no longer borrow the card's thumbnails.
pub fn forget(conn: &Connection, source_id: i64) -> Result<()> {
    let Some((vid, root)): Option<(i64, String)> =
        conn.query_row("SELECT volume_id, rel_root FROM sources WHERE id = ?", [source_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?
    else {
        return Ok(());
    };
    conn.execute(
        "UPDATE files SET clone_of = NULL WHERE clone_of IN (
             SELECT id FROM files WHERE volume_id = ?1 AND (?2 = '' OR substr(rel_path, 1, length(?2) + 1) = ?2 || '/'))",
        params![vid, root],
    )?;
    volumes::remove_source(conn, source_id)?;
    // The card leaves no trace in the drive list either (once nothing of it is indexed).
    conn.execute(
        "DELETE FROM volumes WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM files WHERE volume_id = ?1)
           AND NOT EXISTS (SELECT 1 FROM sources WHERE volume_id = ?1)",
        [vid],
    )?;
    Ok(())
}

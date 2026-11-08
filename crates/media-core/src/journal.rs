//! Every file operation the app performs is journaled so it can be undone.
//!
//! * `quarantine`: soft delete. The file is renamed into a quarantine folder on the *same* volume
//!   (instant, no copy, works on FAT/exFAT USB drives that have no Recycle Bin). Purging the
//!   quarantine is the only permanent delete in the app.
//! * `move`: same volume = rename; cross volume = copy -> BLAKE3 verify -> delete source.
//! * `copy`: copy -> verify; the copy gets its own index row.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::{db, volumes, Db, Progress, ProgressFn, StopFlag};

pub fn new_batch(conn: &Connection, kind: &str, description: &str) -> Result<String> {
    let id = format!("{}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"), &uuid::Uuid::new_v4().simple().to_string()[..6]);
    conn.execute(
        "INSERT INTO batches(id, kind, description, created_at, state) VALUES (?, ?, ?, ?, 'running')",
        params![id, kind, description, crate::now_ts()],
    )?;
    Ok(id)
}

pub fn finish_batch(conn: &Connection, batch_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE batches SET state = 'done', op_count = (SELECT COUNT(*) FROM journal WHERE batch_id = ?1) WHERE id = ?1",
        [batch_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Low-level file helpers
// ---------------------------------------------------------------------------------------------

pub fn hash_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().to_hex().to_string())
}

/// Hash of the first 64 KiB plus the size: cheap pre-filter before full hashing.
pub fn prehash_file(path: &Path, size: u64) -> Result<String> {
    let mut f = fs::File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut filled = 0;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    let mut h = blake3::Hasher::new();
    h.update(&size.to_le_bytes());
    h.update(&buf[..filled]);
    Ok(h.finalize().to_hex()[..32].to_string())
}

fn same_volume(a: &Path, b: &Path) -> bool {
    match (volumes::identify(a), volumes::identify(b)) {
        (Ok(x), Ok(y)) => x.uid == y.uid,
        _ => false,
    }
}

fn existing_ancestor(p: &Path) -> PathBuf {
    let mut cur = p.to_path_buf();
    while !cur.exists() {
        if !cur.pop() {
            break;
        }
    }
    cur
}

/// Copy with verification. Writes to a temp name first so a crash never leaves a half file at `dst`.
pub fn copy_verified(src: &Path, dst: &Path) -> Result<String> {
    if dst.exists() {
        bail!("대상 파일이 이미 존재합니다: {}", dst.display());
    }
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p)?;
    }
    let tmp = dst.with_file_name(format!(
        ".{}.mo-partial",
        dst.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    fs::copy(src, &tmp).with_context(|| format!("copy {} -> {}", src.display(), tmp.display()))?;
    let (hs, hd) = (hash_file(src)?, hash_file(&tmp)?);
    if hs != hd {
        let _ = fs::remove_file(&tmp);
        bail!("복사 검증 실패 (hash mismatch): {}", src.display());
    }
    if let Ok(md) = fs::metadata(src) {
        if let Ok(m) = md.modified() {
            let _ = filetime::set_file_mtime(&tmp, filetime::FileTime::from_system_time(m));
        }
    }
    fs::rename(&tmp, dst)?;
    Ok(hs)
}

/// Move a file; same-volume moves are a rename, cross-volume moves are copy+verify+delete.
pub fn move_file(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        bail!("대상 파일이 이미 존재합니다: {}", dst.display());
    }
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p)?;
    }
    if same_volume(src, &existing_ancestor(dst)) {
        match fs::rename(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) => log::warn!("rename failed ({e}); falling back to copy"),
        }
    }
    copy_verified(src, dst)?;
    fs::remove_file(src).with_context(|| format!("remove {}", src.display()))?;
    Ok(())
}

/// Remove now-empty directories from `start` upward, stopping at `stop_at` (exclusive).
pub fn prune_empty_dirs(start: &Path, stop_at: &Path) {
    let mut cur = start.to_path_buf();
    while cur.starts_with(stop_at) && cur != stop_at {
        if fs::remove_dir(&cur).is_err() {
            break;
        }
        if !cur.pop() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Journaled operations
// ---------------------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn record(
    conn: &Connection,
    batch: &str,
    op: &str,
    file_id: i64,
    src: &Path,
    dst: &Path,
    src_vol: i64,
    src_rel: &str,
    dst_vol: i64,
    dst_rel: &str,
    new_file_id: Option<i64>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO journal(batch_id, op, file_id, src_abs, dst_abs, src_volume, src_rel, dst_volume, dst_rel, new_file_id, ts)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            batch, op, file_id, src.to_string_lossy(), dst.to_string_lossy(), src_vol, src_rel, dst_vol, dst_rel,
            new_file_id, crate::now_ts()
        ],
    )?;
    Ok(())
}

/// Soft-delete one indexed file into its volume's quarantine folder.
pub fn quarantine(conn: &Connection, batch: &str, file: &db::FileRow) -> Result<()> {
    if !file.online {
        bail!("드라이브가 연결되어 있지 않습니다: {}", file.name);
    }
    let src = file.abs_path();
    if !src.exists() {
        bail!("파일이 없습니다: {}", src.display());
    }
    let qroot = volumes::quarantine_root(Path::new(&file.mount));
    let dst = volumes::join_rel(&qroot.join(batch), &format!("v{}/{}", file.volume_id, file.rel_path));
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p)?;
    }
    fs::rename(&src, &dst).with_context(|| format!("quarantine {}", src.display()))?;
    conn.execute("UPDATE files SET quarantined = 1 WHERE id = ?", [file.id])?;
    record(conn, batch, "quarantine", file.id, &src, &dst, file.volume_id, &file.rel_path, file.volume_id, &file.rel_path, None)?;
    Ok(())
}

/// Move an indexed file to `dst` (absolute) and update its index row.
pub fn move_indexed(conn: &Connection, batch: &str, file: &db::FileRow, dst: &Path, dst_vol: i64, dst_rel: &str) -> Result<()> {
    let src = file.abs_path();
    move_file(&src, dst)?;
    conn.execute(
        "UPDATE files SET volume_id = ?, rel_path = ?, name = ? WHERE id = ?",
        params![dst_vol, dst_rel, dst.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(), file.id],
    )?;
    record(conn, batch, "move", file.id, &src, dst, file.volume_id, &file.rel_path, dst_vol, dst_rel, None)?;
    Ok(())
}

/// Copy an indexed file to `dst`, creating a new index row for the copy.
pub fn copy_indexed(conn: &Connection, batch: &str, file: &db::FileRow, dst: &Path, dst_vol: i64, dst_rel: &str) -> Result<i64> {
    let src = file.abs_path();
    let hash = copy_verified(&src, dst)?;
    let name = dst.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    conn.execute(
        "INSERT INTO files(volume_id, rel_path, name, ext, kind, size, mtime, taken_at, date_source, width, height,
                           duration_ms, camera, lat, lon, orientation, hash, prehash, missing, quarantined, scanned_at)
         SELECT ?1, ?2, ?3, ext, kind, size, mtime, taken_at, date_source, width, height, duration_ms, camera, lat, lon,
                orientation, ?4, prehash, 0, 0, scanned_at FROM files WHERE id = ?5
         ON CONFLICT(volume_id, rel_path) DO UPDATE SET missing = 0, quarantined = 0, hash = excluded.hash",
        params![dst_vol, dst_rel, name, hash, file.id],
    )?;
    let new_id: i64 = conn.query_row("SELECT id FROM files WHERE volume_id = ? AND rel_path = ?", params![dst_vol, dst_rel], |r| r.get(0))?;
    // The copy is byte-identical (verified above): it inherits analysis, faces, place, labels and
    // thumbnails instead of being read and analysed again.
    conn.execute(
        "INSERT OR IGNORE INTO labels(file_id, label, source) SELECT ?, label, source FROM labels WHERE file_id = ?",
        params![new_id, file.id],
    )?;
    crate::identity::clone_derived(conn, file.id, new_id)?;
    conn.execute("UPDATE files SET hash = ? WHERE id = ?", params![hash, file.id])?;
    record(conn, batch, "copy", file.id, &src, dst, file.volume_id, &file.rel_path, dst_vol, dst_rel, Some(new_id))?;
    Ok(new_id)
}

// ---------------------------------------------------------------------------------------------
// Batches: list / undo / purge
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct BatchRow {
    pub id: String,
    pub kind: String,
    pub description: String,
    pub created_at: i64,
    pub op_count: i64,
    pub state: String,
    pub bytes: i64,
}

pub fn list_batches(conn: &Connection) -> Result<Vec<BatchRow>> {
    let mut st = conn.prepare(
        "SELECT b.id, b.kind, b.description, b.created_at, b.op_count, b.state,
                COALESCE((SELECT SUM(f.size) FROM journal j JOIN files f ON f.id = COALESCE(j.new_file_id, j.file_id) WHERE j.batch_id = b.id AND j.undone = 0), 0)
         FROM batches b ORDER BY b.created_at DESC, b.id DESC",
    )?;
    let rows = st
        .query_map([], |r| {
            Ok(BatchRow {
                id: r.get(0)?,
                kind: r.get(1)?,
                description: r.get(2)?,
                created_at: r.get(3)?,
                op_count: r.get(4)?,
                state: r.get(5)?,
                bytes: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UndoReport {
    pub restored: u64,
    pub failed: u64,
    pub errors: Vec<String>,
}

struct JournalEntry {
    id: i64,
    op: String,
    file_id: i64,
    src_abs: String,
    dst_abs: String,
    src_volume: i64,
    src_rel: String,
    new_file_id: Option<i64>,
}

/// Reverse a batch, newest operation first.
pub fn undo_batch(conn: &Connection, batch_id: &str) -> Result<UndoReport> {
    let state: Option<String> = conn.query_row("SELECT state FROM batches WHERE id = ?", [batch_id], |r| r.get(0)).optional()?;
    match state.as_deref() {
        None => bail!("작업을 찾을 수 없습니다"),
        Some("purged") => bail!("격리함을 비운 작업은 되돌릴 수 없습니다"),
        Some("undone") => bail!("이미 되돌린 작업입니다"),
        _ => {}
    }
    let entries: Vec<JournalEntry> = {
        let mut st = conn.prepare(
            "SELECT id, op, file_id, src_abs, dst_abs, src_volume, src_rel, new_file_id FROM journal
             WHERE batch_id = ? AND undone = 0 ORDER BY id DESC",
        )?;
        let rows = st
            .query_map([batch_id], |r| {
                Ok(JournalEntry {
                    id: r.get(0)?,
                    op: r.get(1)?,
                    file_id: r.get(2)?,
                    src_abs: r.get(3)?,
                    dst_abs: r.get(4)?,
                    src_volume: r.get(5)?,
                    src_rel: r.get(6)?,
                    new_file_id: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    let mut rep = UndoReport::default();
    for e in entries {
        let (src, dst) = (PathBuf::from(&e.src_abs), PathBuf::from(&e.dst_abs));
        let res: Result<()> = (|| {
            match e.op.as_str() {
                "quarantine" => {
                    if src.exists() {
                        bail!("원래 위치에 같은 이름의 파일이 있습니다: {}", src.display());
                    }
                    if let Some(p) = src.parent() {
                        fs::create_dir_all(p)?;
                    }
                    fs::rename(&dst, &src)?;
                    conn.execute("UPDATE files SET quarantined = 0, missing = 0 WHERE id = ?", [e.file_id])?;
                    if let Some(qroot) = dst.ancestors().find(|a| a.file_name().map(|n| n == batch_id).unwrap_or(false)) {
                        prune_empty_dirs(dst.parent().unwrap_or(qroot), qroot.parent().unwrap_or(qroot));
                    }
                }
                "move" => {
                    move_file(&dst, &src)?;
                    let name = src.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    conn.execute(
                        "UPDATE files SET volume_id = ?, rel_path = ?, name = ?, missing = 0 WHERE id = ?",
                        params![e.src_volume, e.src_rel, name, e.file_id],
                    )?;
                    if let Some(parent) = dst.parent() {
                        // Clean up folders the organizer created, up to the volume root.
                        if let Ok(v) = volumes::identify(&existing_ancestor(parent)) {
                            prune_empty_dirs(parent, &v.root);
                        }
                    }
                }
                "copy" => {
                    if dst.exists() {
                        let hs = hash_file(&src).ok();
                        let hd = hash_file(&dst)?;
                        if hs.as_deref() != Some(hd.as_str()) {
                            bail!("복사본이 수정되어 삭제하지 않았습니다: {}", dst.display());
                        }
                        fs::remove_file(&dst)?;
                    }
                    if let Some(nid) = e.new_file_id {
                        conn.execute("DELETE FROM files WHERE id = ?", [nid])?;
                    }
                    if let Some(parent) = dst.parent() {
                        if let Ok(v) = volumes::identify(&existing_ancestor(parent)) {
                            prune_empty_dirs(parent, &v.root);
                        }
                    }
                }
                other => return Err(anyhow!("unknown op {other}")),
            }
            Ok(())
        })();
        match res {
            Ok(()) => {
                conn.execute("UPDATE journal SET undone = 1 WHERE id = ?", [e.id])?;
                rep.restored += 1;
            }
            Err(err) => {
                rep.failed += 1;
                rep.errors.push(err.to_string());
            }
        }
    }
    let state = if rep.failed == 0 { "undone" } else { "done" };
    conn.execute("UPDATE batches SET state = ? WHERE id = ?", params![state, batch_id])?;
    Ok(rep)
}

/// Permanently delete quarantined files of a batch. This is the only irreversible action.
pub fn purge_batch(conn: &Connection, batch_id: &str) -> Result<UndoReport> {
    let entries: Vec<(i64, i64, String)> = {
        let mut st = conn.prepare("SELECT id, file_id, dst_abs FROM journal WHERE batch_id = ? AND op = 'quarantine' AND undone = 0")?;
        let rows = st.query_map([batch_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    let mut rep = UndoReport::default();
    for (_jid, file_id, dst) in entries {
        let p = PathBuf::from(&dst);
        let r = if p.exists() { fs::remove_file(&p).map_err(anyhow::Error::from) } else { Ok(()) };
        match r {
            Ok(()) => {
                conn.execute("DELETE FROM files WHERE id = ? AND quarantined = 1", [file_id])?;
                rep.restored += 1;
                if let Some(qroot) = p.ancestors().find(|a| a.file_name().map(|n| n == batch_id).unwrap_or(false)) {
                    prune_empty_dirs(p.parent().unwrap_or(qroot), qroot.parent().unwrap_or(qroot));
                }
            }
            Err(e) => {
                rep.failed += 1;
                rep.errors.push(e.to_string());
            }
        }
    }
    if rep.failed == 0 {
        conn.execute("UPDATE batches SET state = 'purged' WHERE id = ?", [batch_id])?;
    }
    Ok(rep)
}

// ---------------------------------------------------------------------------------------------
// Delete selected files (soft: quarantine, undoable)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize)]
pub struct DeleteReport {
    pub batch_id: String,
    pub deleted: u64,
    pub bytes: i64,
    /// On a drive that is not connected right now.
    pub skipped_offline: u64,
    /// Protected (see `protect`) and not forced: left in place.
    pub protected: u64,
    pub failed: u64,
    pub errors: Vec<String>,
    pub cancelled: bool,
}

/// Move the given files into their drives' quarantine folders as one undoable batch.
/// Protected files (originals, ♥, keep folders, the last copy of some content) are left in place
/// unless `force`.
/// `label` names the batch in the history (e.g. "연사 정리").
pub fn delete_files(db: &Db, ids: &[i64], force: bool, label: Option<&str>, stop: &StopFlag, progress: ProgressFn) -> Result<DeleteReport> {
    let conn = db.conn()?;
    volumes::refresh(&conn)?;
    let protected: std::collections::HashSet<i64> =
        if force { Default::default() } else { crate::protect::check(&conn, ids)?.into_iter().map(|b| b.id).collect() };
    let what = match label {
        Some(l) => format!("{l} {}개 정리", ids.len()),
        None => format!("선택한 {}개 삭제", ids.len()),
    };
    let batch = new_batch(&conn, "delete", &what)?;
    let mut rep = DeleteReport { batch_id: batch.clone(), ..Default::default() };
    let total = ids.len() as u64;
    for (i, id) in ids.iter().enumerate() {
        if stop.is_stopped() {
            rep.cancelled = true;
            break;
        }
        if i % 20 == 0 {
            progress(Progress::new("delete", "quarantine", i as u64, total, ""));
        }
        if protected.contains(id) {
            rep.protected += 1;
            continue;
        }
        let Some(f) = db::get_file(&conn, *id)? else { continue };
        if f.quarantined || f.missing {
            continue;
        }
        if !f.online {
            rep.skipped_offline += 1;
            continue;
        }
        match quarantine(&conn, &batch, &f) {
            Ok(()) => {
                rep.deleted += 1;
                rep.bytes += f.size;
            }
            Err(e) => {
                rep.failed += 1;
                if rep.errors.len() < 100 {
                    rep.errors.push(e.to_string());
                }
            }
        }
    }
    finish_batch(&conn, &batch)?;
    if rep.deleted == 0 {
        conn.execute("DELETE FROM batches WHERE id = ? AND op_count = 0", [&batch])?;
    }
    progress(Progress::new("delete", "done", total, total, ""));
    Ok(rep)
}

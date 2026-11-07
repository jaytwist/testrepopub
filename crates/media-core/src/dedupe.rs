//! Duplicate detection.
//!
//! * exact:          computed from our own index (size -> 64 KiB prehash -> full BLAKE3). Hashes are
//!                   stored back in `files.hash`, so re-runs only hash new/changed files.
//! * similar_image:  czkawka_core `SimilarImages` (perceptual hash + BK-tree, with its own cache).
//! * similar_video:  czkawka_core `SimilarVideos` (needs the bundled ffmpeg on PATH).
//!
//! Nothing is deleted here. Groups get a suggested "keep" member; applying a group quarantines
//! the others through the journal (undoable).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;

use anyhow::{anyhow, bail, Result};
use czkawka_core::common::tool_data::CommonData;
use czkawka_core::common::traits::Search;
use czkawka_core::re_exported::{FilterType, HashAlg};
use czkawka_core::tools::similar_images::{
    return_similarity_from_similarity_preset, GeometricInvariance, SimilarImages, SimilarImagesParameters, SimilarityPreset,
};
use czkawka_core::tools::similar_videos::{self as sv, SimilarVideos, SimilarVideosParameters};
use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::{FileRow, FILE_COLS};
use crate::{journal, scanner, volumes, Db, Progress, ProgressFn, StopFlag};

static CZ_INIT: Once = Once::new();

/// czkawka_core keeps its hash caches in a config/cache folder that must be set exactly once.
pub fn init_czkawka(cache_dir: &Path) {
    CZ_INIT.call_once(|| {
        let c = cache_dir.join("czkawka");
        let _ = std::fs::create_dir_all(&c);
        // SAFETY: called once at startup before any worker threads read the environment.
        unsafe {
            std::env::set_var("CZKAWKA_CACHE_PATH", &c);
            std::env::set_var("CZKAWKA_CONFIG_PATH", &c);
        }
        let _ = czkawka_core::common::config_cache_path::set_config_cache_path("MediaOrganizer", "MediaOrganizer");
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DupKind {
    Exact,
    SimilarImage,
    SimilarVideo,
}

impl DupKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DupKind::Exact => "exact",
            DupKind::SimilarImage => "similar_image",
            DupKind::SimilarVideo => "similar_video",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DedupeStats {
    pub kind: String,
    pub groups: u64,
    pub files: u64,
    pub reclaimable_bytes: i64,
}

fn online_files_by_size(conn: &Connection) -> Result<Vec<FileRow>> {
    let sql = format!(
        "SELECT {FILE_COLS} FROM files f JOIN volumes v ON v.id = f.volume_id
         WHERE v.online = 1 AND f.missing = 0 AND f.quarantined = 0 AND f.size > 0
           AND f.size IN (SELECT f2.size FROM files f2 JOIN volumes v2 ON v2.id = f2.volume_id
                          WHERE v2.online = 1 AND f2.missing = 0 AND f2.quarantined = 0
                          GROUP BY f2.size HAVING COUNT(*) > 1)
         ORDER BY f.size"
    );
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map([], FileRow::from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Exact (byte-identical) duplicates across every online drive.
pub fn find_exact(db: &Db, stop: &StopFlag, progress: ProgressFn) -> Result<DedupeStats> {
    let job = "dedupe:exact";
    let mut conn = db.conn()?;
    volumes::refresh(&conn)?;
    let cands = online_files_by_size(&conn)?;

    // Group by size, then prehash within each size group.
    let mut by_size: HashMap<i64, Vec<FileRow>> = HashMap::new();
    for f in cands {
        by_size.entry(f.size).or_default().push(f);
    }
    // Repeat runs must be quick: a file with a unique size can't have a copy (no read at all), and a
    // stored full hash is still valid (the scanner clears it when the file changes). Only groups
    // where nobody has a full hash yet are narrowed down with a 64 KiB prehash; a new file next to
    // already-hashed ones is fully hashed directly below.
    let size_groups: Vec<Vec<FileRow>> = by_size.into_values().filter(|g| g.len() > 1).collect();
    let pre_total: u64 = size_groups.iter().filter(|g| g.iter().all(|f| f.hash.is_none())).map(|g| g.len() as u64).sum();
    let done = AtomicU64::new(0);
    progress(Progress::new(job, "prehash", 0, pre_total, ""));
    // Kept: the same fingerprint recognises these files later when moved or copied.
    let fresh_prehashes = std::sync::Mutex::new(Vec::<(i64, String)>::new());
    let pre_groups: Vec<Vec<FileRow>> = size_groups
        .into_par_iter()
        .flat_map(|group| {
            if stop.is_stopped() {
                return Vec::new();
            }
            if group.iter().any(|f| f.hash.is_some()) {
                return vec![group];
            }
            let mut m: HashMap<String, Vec<FileRow>> = HashMap::new();
            for f in group {
                let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                if d % 200 == 0 {
                    progress(Progress::new(job, "prehash", d, pre_total, f.name.clone()));
                }
                if let Ok(p) = journal::prehash_file(&f.abs_path(), f.size as u64) {
                    fresh_prehashes.lock().unwrap().push((f.id, p.clone()));
                    m.entry(p).or_default().push(f);
                }
            }
            // Only groups with >1 member can contain duplicates.
            m.into_values().filter(|v| v.len() > 1).collect::<Vec<_>>()
        })
        .collect();
    if stop.is_stopped() {
        bail!("cancelled");
    }

    // Full hash for everything still ambiguous.
    let to_hash: Vec<&FileRow> = pre_groups.iter().flatten().filter(|f| f.hash.is_none()).collect();
    let total = to_hash.len() as u64;
    let done = AtomicU64::new(0);
    progress(Progress::new(job, "hash", 0, total, ""));
    let hashed: HashMap<i64, String> = to_hash
        .par_iter()
        .filter_map(|f| {
            if stop.is_stopped() {
                return None;
            }
            let h = journal::hash_file(&f.abs_path()).ok()?;
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            if d % 20 == 0 {
                progress(Progress::new(job, "hash", d, total, f.name.clone()));
            }
            Some((f.id, h))
        })
        .collect();
    if stop.is_stopped() {
        bail!("cancelled");
    }
    {
        let tx = conn.transaction()?;
        {
            let mut st = tx.prepare_cached("UPDATE files SET hash = ? WHERE id = ?")?;
            for (id, h) in &hashed {
                st.execute(params![h, id])?;
            }
            let mut st = tx.prepare_cached("UPDATE files SET prehash = ? WHERE id = ? AND prehash IS NULL")?;
            for (id, p) in fresh_prehashes.into_inner().unwrap() {
                st.execute(params![p, id])?;
            }
        }
        tx.commit()?;
    }

    let mut by_hash: HashMap<String, Vec<FileRow>> = HashMap::new();
    for f in pre_groups.into_iter().flatten() {
        let h = f.hash.clone().or_else(|| hashed.get(&f.id).cloned());
        if let Some(h) = h {
            by_hash.entry(h).or_default().push(f);
        }
    }
    let groups: Vec<Vec<FileRow>> = by_hash.into_values().filter(|g| g.len() > 1).collect();
    save_groups(&mut conn, DupKind::Exact, groups)
}

fn source_roots(conn: &Connection) -> Result<Vec<PathBuf>> {
    Ok(crate::db::list_sources(conn)?.into_iter().filter(|s| s.online).map(|s| PathBuf::from(s.abs_path)).collect())
}

fn quarantine_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = roots
        .iter()
        .filter_map(|r| volumes::identify(r).ok())
        .map(|v| volumes::quarantine_root(&v.root))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn forward_progress<'a>(
    job: &'static str,
    progress: ProgressFn<'a>,
) -> (crossbeam_channel::Sender<czkawka_core::common::progress_data::ProgressData>, impl FnOnce() + Send + 'a) {
    let (tx, rx) = crossbeam_channel::unbounded::<czkawka_core::common::progress_data::ProgressData>();
    // Bridge czkawka progress into ours on a scoped helper; `finish` drains remaining messages.
    let drain = move || {
        while let Ok(p) = rx.recv() {
            let (done, total) = if p.bytes_to_check > 0 {
                (p.bytes_checked, p.bytes_to_check)
            } else {
                (p.entries_checked as u64, p.entries_to_check as u64)
            };
            progress(Progress::new(job, &format!("{:?}", p.stage), done, total, ""));
        }
    };
    (tx, drain)
}

/// Similarity presets exposed to the UI (maps to czkawka's presets at hash size 16).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Similarity {
    VeryHigh,
    High,
    Medium,
}

/// Visually similar images (resized, recompressed, lightly edited copies).
pub fn find_similar_images(db: &Db, similarity: Similarity, stop: &StopFlag, progress: ProgressFn) -> Result<DedupeStats> {
    let mut conn = db.conn()?;
    volumes::refresh(&conn)?;
    let roots = source_roots(&conn)?;
    if roots.is_empty() {
        bail!("연결된 소스 폴더가 없습니다");
    }
    let preset = match similarity {
        Similarity::VeryHigh => SimilarityPreset::VeryHigh,
        Similarity::High => SimilarityPreset::High,
        Similarity::Medium => SimilarityPreset::Medium,
    };
    let hash_size = 16;
    let params = SimilarImagesParameters::new(
        return_similarity_from_similarity_preset(preset, hash_size),
        hash_size,
        HashAlg::Gradient,
        FilterType::Lanczos3,
        false,
        false,
        GeometricInvariance::Off,
    );
    let mut finder = SimilarImages::new(params);
    finder.set_included_paths(roots.clone());
    finder.set_excluded_paths(quarantine_roots(&roots));
    finder.set_recursive_search(true);
    finder.set_use_cache(true);
    finder.set_minimal_file_size(10 * 1024);

    let results: Vec<Vec<PathBuf>> = std::thread::scope(|s| {
        let (tx, drain) = forward_progress("dedupe:similar_image", progress);
        let h = s.spawn(drain);
        finder.search(stop.inner(), Some(&tx));
        drop(tx);
        let _ = h.join();
        finder.get_similar_images().iter().map(|g| g.iter().map(|e| e.path.clone()).collect()).collect()
    });
    if stop.is_stopped() {
        bail!("cancelled");
    }
    let groups = map_paths_to_rows(&conn, results)?;
    save_groups(&mut conn, DupKind::SimilarImage, groups)
}

pub fn ffmpeg_available() -> bool {
    let mut cmd = std::process::Command::new("ffprobe");
    cmd.arg("-version").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    crate::thumbs::hide_console(&mut cmd);
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// Re-encoded / trimmed copies of the same video.
pub fn find_similar_videos(db: &Db, stop: &StopFlag, progress: ProgressFn) -> Result<DedupeStats> {
    if !ffmpeg_available() {
        bail!("ffmpeg/ffprobe를 찾을 수 없습니다. 설치 파일에 포함된 ffmpeg가 필요합니다.");
    }
    let mut conn = db.conn()?;
    volumes::refresh(&conn)?;
    let roots = source_roots(&conn)?;
    if roots.is_empty() {
        bail!("연결된 소스 폴더가 없습니다");
    }
    let params = SimilarVideosParameters::new(
        10,
        false,
        false,
        sv::DEFAULT_SKIP_FORWARD_AMOUNT,
        sv::DEFAULT_VID_HASH_DURATION,
        sv::DEFAULT_CROP_DETECT,
        sv::DEFAULT_WINDOW_COUNT,
        sv::DEFAULT_DURATION_TOLERANCE_PCT,
        sv::DEFAULT_MIN_MATCHING_WINDOWS,
        sv::DEFAULT_SUBCLIP_MIN_MATCH,
        false,
        sv::DEFAULT_VIDEO_PERCENTAGE_FOR_THUMBNAIL,
        false,
        sv::DEFAULT_THUMBNAIL_GRID_TILES_PER_SIDE,
        false,
        sv::DEFAULT_AUDIO_SIMILARITY_PERCENT,
        sv::DEFAULT_AUDIO_MAXIMUM_DIFFERENCE,
        sv::DEFAULT_AUDIO_LENGTH_RATIO,
        sv::DEFAULT_AUDIO_MIN_DURATION_SECONDS,
    );
    let mut finder = SimilarVideos::new(params);
    finder.set_included_paths(roots.clone());
    finder.set_excluded_paths(quarantine_roots(&roots));
    finder.set_recursive_search(true);
    finder.set_use_cache(true);

    let results: Vec<Vec<PathBuf>> = std::thread::scope(|s| {
        let (tx, drain) = forward_progress("dedupe:similar_video", progress);
        let h = s.spawn(drain);
        finder.search(stop.inner(), Some(&tx));
        drop(tx);
        let _ = h.join();
        finder.get_similar_videos().iter().map(|g| g.iter().map(|e| e.path.clone()).collect()).collect()
    });
    if stop.is_stopped() {
        bail!("cancelled");
    }
    let groups = map_paths_to_rows(&conn, results)?;
    save_groups(&mut conn, DupKind::SimilarVideo, groups)
}

/// Similar-groups that only contain byte-identical copies are already covered by the exact tool;
/// showing them twice would double-count reclaimable space.
fn drop_identical_only(groups: Vec<Vec<FileRow>>) -> Vec<Vec<FileRow>> {
    groups
        .into_iter()
        .filter(|g| {
            let sizes: std::collections::HashSet<i64> = g.iter().map(|f| f.size).collect();
            if sizes.len() > 1 {
                return true;
            }
            let hashes: std::collections::HashSet<String> =
                g.iter().filter_map(|f| f.hash.clone().or_else(|| journal::hash_file(&f.abs_path()).ok())).collect();
            hashes.len() > 1
        })
        .collect()
}

fn map_paths_to_rows(conn: &Connection, groups: Vec<Vec<PathBuf>>) -> Result<Vec<Vec<FileRow>>> {
    let index = scanner::abs_path_index(conn)?;
    let mut out = Vec::new();
    for g in groups {
        let rows: Vec<FileRow> = g
            .iter()
            .filter_map(|p| {
                let key = volumes::canonical(p).unwrap_or_else(|_| p.clone());
                index.get(&key).or_else(|| index.get(p)).copied()
            })
            .filter_map(|id| crate::db::get_file(conn, id).ok().flatten())
            .collect();
        if rows.len() > 1 {
            out.push(rows);
        }
    }
    Ok(drop_identical_only(out))
}

/// The user's "which copy to keep" rules for duplicates, by folder:
/// copies inside a `keep` folder win; copies inside a `drop` folder lose (one is still kept when
/// every copy is in a drop folder). Stored in `settings` as `dup_rules`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DupRules {
    pub keep: Vec<String>,
    pub drop: Vec<String>,
}

fn norm_path(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

impl DupRules {
    pub fn load(conn: &Connection) -> Result<Self> {
        let v: Option<String> = conn
            .query_row("SELECT value FROM settings WHERE key = 'dup_rules'", [], |r| r.get(0))
            .optional()?;
        Ok(v.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
    }

    pub fn save(&self, conn: &Connection) -> Result<()> {
        conn.execute(
            "INSERT INTO settings(key, value) VALUES ('dup_rules', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [serde_json::to_string(self)?],
        )?;
        Ok(())
    }

    /// 0 = in a keep folder, 1 = no rule, 2 = in a drop folder.
    fn class(&self, f: &FileRow) -> u8 {
        let p = norm_path(&f.abs_path().to_string_lossy());
        let inside = |dirs: &[String]| {
            dirs.iter().map(|d| norm_path(d)).any(|d| !d.is_empty() && (p == d || p.starts_with(&format!("{d}/"))))
        };
        if inside(&self.keep) {
            0
        } else if inside(&self.drop) {
            2
        } else {
            1
        }
    }
}

/// Rank group members: the first one is the suggested keeper.
/// The user's folder rules first; then highest resolution, trustworthy date, not-from-messenger,
/// oldest, shortest path.
pub fn rank_members(members: &mut [FileRow], labels: &HashMap<i64, Vec<String>>, rules: &DupRules) {
    let score = |f: &FileRow| {
        let pixels = f.width.unwrap_or(0).saturating_mul(f.height.unwrap_or(0));
        let date_rank = match crate::meta::confidence(&f.date_source) {
            "high" => 0,
            "medium" => 1,
            _ => 2,
        };
        let ls = labels.get(&f.id);
        let messenger = ls.map(|l| l.iter().any(|x| x.starts_with("src:"))).unwrap_or(false) as i32;
        let has_camera = f.camera.is_some() as i32;
        (rules.class(f), std::cmp::Reverse(pixels), std::cmp::Reverse(f.size), date_rank, messenger, std::cmp::Reverse(has_camera), f.mtime, f.rel_path.len())
    };
    members.sort_by_key(score);
}

/// Re-pick the keeper of every unresolved group with the current rules (after the user changed
/// them). Returns how many groups got a different keeper.
pub fn apply_rules(conn: &mut Connection) -> Result<usize> {
    let rules = DupRules::load(conn)?;
    let groups: Vec<i64> = {
        let mut st = conn.prepare("SELECT id FROM dup_groups WHERE resolved = 0")?;
        let rows = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
        rows
    };
    let sql = format!(
        "SELECT {FILE_COLS}, m.keep FROM dup_members m JOIN files f ON f.id = m.file_id JOIN volumes v ON v.id = f.volume_id
         WHERE m.group_id = ? AND f.quarantined = 0 AND f.missing = 0"
    );
    let tx = conn.transaction()?;
    let mut changed = 0;
    {
        let mut st = tx.prepare(&sql)?;
        let mut lab = tx.prepare("SELECT label FROM labels WHERE file_id = ?")?;
        let mut set = tx.prepare("UPDATE dup_members SET keep = ? WHERE group_id = ? AND file_id = ?")?;
        for gid in groups {
            let rows: Vec<(FileRow, bool)> = st
                .query_map([gid], |r| Ok((FileRow::from_row(r)?, r.get::<_, i64>(crate::db::FILE_COLS_LEN)? != 0)))?
                .collect::<rusqlite::Result<_>>()?;
            if rows.len() < 2 {
                continue;
            }
            let before: Vec<i64> = rows.iter().filter(|(_, k)| *k).map(|(f, _)| f.id).collect();
            let mut files: Vec<FileRow> = rows.into_iter().map(|(f, _)| f).collect();
            let mut labels = HashMap::new();
            for f in &files {
                labels.insert(f.id, lab.query_map([f.id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?);
            }
            rank_members(&mut files, &labels, &rules);
            if before != vec![files[0].id] {
                changed += 1;
            }
            for (i, f) in files.iter().enumerate() {
                set.execute(params![(i == 0) as i64, gid, f.id])?;
            }
        }
    }
    tx.commit()?;
    Ok(changed)
}

fn save_groups(conn: &mut Connection, kind: DupKind, groups: Vec<Vec<FileRow>>) -> Result<DedupeStats> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM dup_groups WHERE kind = ? AND resolved = 0", [kind.as_str()])?;
    let mut stats = DedupeStats { kind: kind.as_str().into(), ..Default::default() };
    let now = crate::now_ts();
    {
        let mut ins_g = tx.prepare("INSERT INTO dup_groups(kind, created_at) VALUES (?, ?)")?;
        let mut ins_m = tx.prepare("INSERT OR IGNORE INTO dup_members(group_id, file_id, keep) VALUES (?, ?, ?)")?;
        let mut lab = tx.prepare("SELECT label FROM labels WHERE file_id = ?")?;
        let rules = DupRules::load(&tx)?;
        for mut g in groups {
            let mut labels = HashMap::new();
            for f in &g {
                let ls: Vec<String> = lab.query_map([f.id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
                labels.insert(f.id, ls);
            }
            rank_members(&mut g, &labels, &rules);
            ins_g.execute(params![kind.as_str(), now])?;
            let gid = tx.last_insert_rowid();
            for (i, f) in g.iter().enumerate() {
                ins_m.execute(params![gid, f.id, (i == 0) as i64])?;
                if i > 0 {
                    stats.reclaimable_bytes += f.size;
                }
            }
            stats.groups += 1;
            stats.files += g.len() as u64;
        }
    }
    tx.commit()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------------------------
// Review / apply
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct DupMember {
    pub file: FileRow,
    pub keep: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct DupGroup {
    pub id: i64,
    pub kind: String,
    pub members: Vec<DupMember>,
    pub reclaimable_bytes: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct DupSummary {
    pub kind: String,
    pub groups: i64,
    pub files: i64,
    pub reclaimable_bytes: i64,
}

pub fn summary(conn: &Connection) -> Result<Vec<DupSummary>> {
    let mut st = conn.prepare(
        "SELECT g.kind, COUNT(DISTINCT g.id), COUNT(*), COALESCE(SUM(CASE WHEN m.keep = 0 THEN f.size END), 0)
         FROM dup_groups g JOIN dup_members m ON m.group_id = g.id JOIN files f ON f.id = m.file_id
         WHERE g.resolved = 0 AND f.quarantined = 0 AND f.missing = 0 GROUP BY g.kind",
    )?;
    let rows = st
        .query_map([], |r| Ok(DupSummary { kind: r.get(0)?, groups: r.get(1)?, files: r.get(2)?, reclaimable_bytes: r.get(3)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn list_groups(conn: &Connection, kind: &str, offset: i64, limit: i64) -> Result<Vec<DupGroup>> {
    let ids: Vec<i64> = {
        let mut st = conn.prepare(
            "SELECT g.id FROM dup_groups g WHERE g.kind = ? AND g.resolved = 0
               AND (SELECT COUNT(*) FROM dup_members m JOIN files f ON f.id = m.file_id
                    WHERE m.group_id = g.id AND f.quarantined = 0 AND f.missing = 0) > 1
             ORDER BY (SELECT MAX(f.size) FROM dup_members m JOIN files f ON f.id = m.file_id WHERE m.group_id = g.id) DESC
             LIMIT ? OFFSET ?",
        )?;
        let rows = st.query_map(params![kind, limit, offset], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    let sql = format!(
        "SELECT {FILE_COLS}, m.keep FROM dup_members m JOIN files f ON f.id = m.file_id JOIN volumes v ON v.id = f.volume_id
         WHERE m.group_id = ? AND f.quarantined = 0 AND f.missing = 0 ORDER BY m.keep DESC, f.size DESC"
    );
    let mut st = conn.prepare(&sql)?;
    let mut out = Vec::new();
    for id in ids {
        let members: Vec<DupMember> = st
            .query_map([id], |r| Ok(DupMember { file: FileRow::from_row(r)?, keep: r.get::<_, i64>(crate::db::FILE_COLS_LEN)? != 0 }))?
            .collect::<rusqlite::Result<_>>()?;
        let reclaimable_bytes = members.iter().filter(|m| !m.keep).map(|m| m.file.size).sum();
        out.push(DupGroup { id, kind: kind.to_string(), members, reclaimable_bytes });
    }
    Ok(out)
}

pub fn set_keep(conn: &Connection, group_id: i64, file_id: i64, keep: bool) -> Result<()> {
    conn.execute("UPDATE dup_members SET keep = ? WHERE group_id = ? AND file_id = ?", params![keep as i64, group_id, file_id])?;
    Ok(())
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ApplyReport {
    pub batch_id: String,
    pub quarantined: u64,
    pub bytes: i64,
    pub skipped_groups: u64,
    pub errors: Vec<String>,
}

/// Quarantine every non-kept member of the given groups (or all unresolved groups of `kind`).
pub fn apply(db: &Db, kind: &str, group_ids: Option<Vec<i64>>, stop: &StopFlag, progress: ProgressFn) -> Result<ApplyReport> {
    let conn = db.conn()?;
    volumes::refresh(&conn)?;
    let ids: Vec<i64> = match group_ids {
        Some(v) => v,
        None => {
            let mut st = conn.prepare("SELECT id FROM dup_groups WHERE kind = ? AND resolved = 0")?;
            let rows = st.query_map([kind], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            rows
        }
    };
    let label = match kind {
        "exact" => "완전 중복",
        "similar_image" => "유사 사진",
        _ => "유사 영상",
    };
    let batch = journal::new_batch(&conn, "dedupe", &format!("{label} 정리 ({}개 그룹)", ids.len()))?;
    let mut rep = ApplyReport { batch_id: batch.clone(), ..Default::default() };
    let sql = format!(
        "SELECT {FILE_COLS}, m.keep FROM dup_members m JOIN files f ON f.id = m.file_id JOIN volumes v ON v.id = f.volume_id
         WHERE m.group_id = ? AND f.quarantined = 0 AND f.missing = 0"
    );
    let total = ids.len() as u64;
    for (i, gid) in ids.iter().enumerate() {
        if stop.is_stopped() {
            break;
        }
        progress(Progress::new("dedupe:apply", "quarantine", i as u64, total, ""));
        let members: Vec<(FileRow, bool)> = {
            let mut st = conn.prepare_cached(&sql)?;
            let rows = st.query_map([gid], |r| Ok((FileRow::from_row(r)?, r.get::<_, i64>(crate::db::FILE_COLS_LEN)? != 0)))?.collect::<rusqlite::Result<_>>()?;
            rows
        };
        let keepers: Vec<&FileRow> = members.iter().filter(|(_, k)| *k).map(|(f, _)| f).collect();
        // Safety: never empty a group; the keeper must exist on disk right now.
        let keeper_ok = keepers.iter().any(|f| f.online && f.abs_path().is_file());
        if !keeper_ok {
            rep.skipped_groups += 1;
            rep.errors.push(format!("그룹 {gid}: 보존할 파일이 없거나 연결되지 않아 건너뜀"));
            continue;
        }
        for (f, keep) in &members {
            if *keep {
                continue;
            }
            match journal::quarantine(&conn, &batch, f) {
                Ok(()) => {
                    rep.quarantined += 1;
                    rep.bytes += f.size;
                }
                Err(e) => rep.errors.push(e.to_string()),
            }
        }
        conn.execute("UPDATE dup_groups SET resolved = 1 WHERE id = ?", [gid])?;
    }
    journal::finish_batch(&conn, &batch)?;
    if rep.quarantined == 0 {
        conn.execute("DELETE FROM batches WHERE id = ? AND op_count = 0", [&batch])?;
    }
    Ok(rep)
}

pub fn run(db: &Db, kind: DupKind, similarity: Similarity, stop: &StopFlag, progress: ProgressFn) -> Result<DedupeStats> {
    match kind {
        DupKind::Exact => find_exact(db, stop, progress),
        DupKind::SimilarImage => find_similar_images(db, similarity, stop, progress),
        DupKind::SimilarVideo => find_similar_videos(db, stop, progress),
    }
    .map_err(|e| anyhow!("{e:#}"))
}

//! Incremental scanner: walks a source folder, finds media, extracts metadata for new or changed
//! files in parallel, and streams results into SQLite in batches (so the timeline fills while the
//! scan is still running).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, Result};
use rayon::prelude::*;
use rusqlite::{params, Connection};
use serde::Serialize;
use walkdir::WalkDir;

use crate::media::{ext_of, kind_for_ext, Kind};
use crate::userfiles::{AppDirDetector, Rules, Skip, SkipCounts};
use crate::{labels, meta, volumes, Db, Progress, ProgressFn, StopFlag};

pub use crate::userfiles::ScanOptions;

#[derive(Clone, Debug, Default, Serialize)]
pub struct ScanStats {
    pub source_id: i64,
    pub seen: u64,
    pub added: u64,
    pub updated: u64,
    pub unchanged: u64,
    pub missing: u64,
    pub errors: u64,
    /// What was deliberately not indexed, and why.
    pub skipped: SkipCounts,
}

struct Candidate {
    rel: String,
    abs: PathBuf,
    name: String,
    ext: String,
    kind: Kind,
    size: u64,
    mtime: i64,
}

fn mtime_of(md: &std::fs::Metadata) -> i64 {
    md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Scan one registered source.
pub fn scan_source(db: &Db, source_id: i64, opts: &ScanOptions, stop: &StopFlag, progress: ProgressFn) -> Result<ScanStats> {
    let mut conn = db.conn()?;
    volumes::refresh(&conn)?;
    let (volume_id, rel_root, mount, online): (i64, String, String, i64) = conn.query_row(
        "SELECT s.volume_id, s.rel_root, v.mount, v.online FROM sources s JOIN volumes v ON v.id = s.volume_id WHERE s.id = ?",
        [source_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    if online == 0 {
        return Err(anyhow!("드라이브가 연결되어 있지 않습니다 (offline volume)"));
    }
    let vol_root = PathBuf::from(&mount);
    let abs_root = volumes::join_rel(&vol_root, &rel_root);
    if !abs_root.is_dir() {
        return Err(anyhow!("폴더를 찾을 수 없습니다: {}", abs_root.display()));
    }

    let job = format!("scan:{source_id}");
    let mut stats = ScanStats { source_id, ..Default::default() };

    // 1. Walk, skipping anything that is not the user's own media.
    let rules = Rules::new(opts);
    let skipped = std::cell::RefCell::new(SkipCounts::default());
    let mut apps = AppDirDetector::default();
    let mut cands: Vec<Candidate> = Vec::new();
    let mut walked: u64 = 0;
    let walker = WalkDir::new(&abs_root).follow_links(false).into_iter().filter_entry(|e| {
        if e.depth() == 0 || !e.file_type().is_dir() {
            return true;
        }
        let md = e.metadata().ok();
        match rules.dir_skip(e.path(), md.as_ref()) {
            Some(reason) => {
                skipped.borrow_mut().add(reason);
                false
            }
            None => true,
        }
    });
    for entry in walker {
        if stop.is_stopped() {
            return Err(anyhow!("cancelled"));
        }
        let Ok(entry) = entry else {
            stats.errors += 1;
            continue;
        };
        walked += 1;
        if walked % 2000 == 0 {
            progress(Progress::new(&job, "walk", cands.len() as u64, 0, entry.path().to_string_lossy()));
        }
        if !entry.file_type().is_file() {
            continue;
        }
        if opts.skip_app_folders {
            apps.observe(entry.path());
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("._") {
            continue; // macOS resource forks on exFAT drives
        }
        let ext = ext_of(&name);
        let Some(kind) = kind_for_ext(&ext) else { continue };
        let Ok(md) = entry.metadata() else {
            stats.errors += 1;
            continue;
        };
        let size = md.len();
        if let Some(reason) = rules.file_skip(&name, kind == Kind::Video, size, &md) {
            skipped.borrow_mut().add(reason);
            continue;
        }
        let Ok(rel) = volumes::rel_of(&vol_root, entry.path()) else { continue };
        cands.push(Candidate { rel, abs: entry.path().to_path_buf(), name, ext, kind, size, mtime: mtime_of(&md) });
    }
    // Media shipped inside installed programs and games (icons, textures, sample clips).
    if opts.skip_app_folders {
        let dirs = apps.app_dirs();
        if !dirs.is_empty() {
            let before = cands.len();
            cands.retain(|c| !AppDirDetector::contains(&dirs, &abs_root, &c.abs));
            skipped.borrow_mut().app_files += (before - cands.len()) as u64;
        }
    }
    stats.skipped = skipped.into_inner();
    stats.seen = cands.len() as u64;

    // 2. Diff against the index.
    let existing: HashMap<String, (i64, i64, i64)> = {
        let mut st = conn.prepare(
            "SELECT rel_path, id, size, mtime FROM files WHERE volume_id = ?1
             AND (?2 = '' OR substr(rel_path, 1, length(?2) + 1) = ?2 || '/')",
        )?;
        let rows = st
            .query_map(params![volume_id, rel_root], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        rows
    };
    let mut todo: Vec<&Candidate> = Vec::new();
    let mut unchanged_ids: Vec<i64> = Vec::new();
    for c in &cands {
        match existing.get(&c.rel) {
            Some((id, size, mtime)) if *size == c.size as i64 && *mtime == c.mtime => unchanged_ids.push(*id),
            _ => todo.push(c),
        }
    }
    stats.unchanged = unchanged_ids.len() as u64;

    // 3. Extract metadata in parallel, write in batches.
    let total = todo.len() as u64;
    let done = AtomicU64::new(0);
    progress(Progress::new(&job, "meta", 0, total, ""));
    let now = crate::now_ts();
    for chunk in todo.chunks(400) {
        if stop.is_stopped() {
            return Err(anyhow!("cancelled"));
        }
        let metas: Vec<(&Candidate, meta::Meta)> = chunk
            .par_iter()
            .map(|c| {
                let m = meta::extract(&c.abs, c.kind, c.mtime);
                let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                if d % 50 == 0 {
                    progress(Progress::new(&job, "meta", d, total, c.name.clone()));
                }
                (*c, m)
            })
            .collect();
        // Icon-sized pictures (stickers, emoji, web thumbnails) are not the user's photos.
        let (metas, small): (Vec<_>, Vec<_>) =
            metas.into_iter().partition(|(c, m)| c.kind != Kind::Photo || !rules.pixels_too_small(m.width, m.height));
        for (c, _) in &small {
            stats.skipped.add(Skip::TooSmall);
            if let Some((id, _, _)) = existing.get(&c.rel) {
                conn.execute("UPDATE files SET missing = 1 WHERE id = ?", [id])?;
            }
        }
        stats.seen -= small.len() as u64;
        let tx = conn.transaction()?;
        {
            let mut up = tx.prepare_cached(
                "INSERT INTO files(volume_id, rel_path, name, ext, kind, size, mtime, taken_at, date_source, width, height,
                                   duration_ms, camera, lat, lon, orientation, hash, missing, quarantined, scanned_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, NULL, 0, 0, ?17)
                 ON CONFLICT(volume_id, rel_path) DO UPDATE SET
                    name=excluded.name, ext=excluded.ext, kind=excluded.kind, size=excluded.size, mtime=excluded.mtime,
                    taken_at = CASE WHEN files.date_source = 'user' THEN files.taken_at ELSE excluded.taken_at END,
                    date_source = CASE WHEN files.date_source = 'user' THEN 'user' ELSE excluded.date_source END,
                    width=excluded.width, height=excluded.height, duration_ms=excluded.duration_ms, camera=excluded.camera,
                    lat=excluded.lat, lon=excluded.lon, orientation=excluded.orientation, hash=NULL, missing=0,
                    quarantined=0, scanned_at=excluded.scanned_at
                 RETURNING id",
            )?;
            for (c, m) in &metas {
                let id: i64 = up.query_row(
                    params![
                        volume_id, c.rel, c.name, c.ext, c.kind.as_str(), c.size as i64, c.mtime, m.taken_at, m.date_source,
                        m.width, m.height, m.duration_ms, m.camera, m.lat, m.lon, m.orientation, now
                    ],
                    |r| r.get(0),
                )?;
                if existing.contains_key(&c.rel) {
                    stats.updated += 1;
                } else {
                    stats.added += 1;
                }
                let ls = labels::rule_labels(&c.rel, &c.name, c.kind.as_str(), m);
                labels::replace_rule_labels(&tx, id, &ls)?;
            }
        }
        tx.commit()?;
    }

    // 4. Unchanged files are present again (they may have been marked missing before).
    {
        let tx = conn.transaction()?;
        {
            let mut st = tx.prepare_cached("UPDATE files SET missing = 0 WHERE id = ? AND missing = 1")?;
            for id in &unchanged_ids {
                st.execute([id])?;
            }
        }
        tx.commit()?;
    }

    // 5. Anything indexed under this root that we did not see is gone (moved/deleted outside the app).
    let seen: std::collections::HashSet<&str> = cands.iter().map(|c| c.rel.as_str()).collect();
    let gone: Vec<i64> = existing.iter().filter(|(rel, _)| !seen.contains(rel.as_str())).map(|(_, (id, _, _))| *id).collect();
    {
        let tx = conn.transaction()?;
        {
            let mut st = tx.prepare_cached("UPDATE files SET missing = 1 WHERE id = ? AND quarantined = 0")?;
            for id in &gone {
                stats.missing += st.execute([id])? as u64;
            }
        }
        tx.commit()?;
    }
    // Settings may have changed since these were indexed: hide photos now below the size threshold.
    stats.skipped.too_small += conn.execute(
        "UPDATE files SET missing = 1 WHERE volume_id = ?1 AND missing = 0 AND quarantined = 0 AND kind = 'photo'
           AND width > 0 AND height > 0 AND MAX(width, height) < ?3
           AND (?2 = '' OR substr(rel_path, 1, length(?2) + 1) = ?2 || '/')",
        params![volume_id, rel_root, opts.min_image_side],
    )? as u64;
    conn.execute("UPDATE sources SET last_scan = ? WHERE id = ?", params![now, source_id])?;
    progress(Progress::new(&job, "done", total, total, format!("{} files", stats.seen)));
    Ok(stats)
}

/// Scan every source whose volume is currently online.
pub fn scan_all(db: &Db, opts: &ScanOptions, stop: &StopFlag, progress: ProgressFn) -> Result<Vec<ScanStats>> {
    let conn = db.conn()?;
    volumes::refresh(&conn)?;
    let ids: Vec<i64> = {
        let mut st = conn.prepare("SELECT s.id FROM sources s JOIN volumes v ON v.id = s.volume_id WHERE v.online = 1")?;
        let rows = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    drop(conn);
    let mut out = Vec::new();
    for id in ids {
        if stop.is_stopped() {
            break;
        }
        out.push(scan_source(db, id, opts, stop, progress)?);
    }
    Ok(out)
}

/// Lookup table from absolute path (as produced by czkawka) to file id, for online files.
pub fn abs_path_index(conn: &Connection) -> Result<HashMap<PathBuf, i64>> {
    let mut st = conn.prepare(
        "SELECT f.id, v.mount, f.rel_path FROM files f JOIN volumes v ON v.id = f.volume_id
         WHERE v.online = 1 AND f.missing = 0 AND f.quarantined = 0",
    )?;
    let rows = st
        .query_map([], |r| {
            let mount: String = r.get(1)?;
            let rel: String = r.get(2)?;
            Ok((volumes::join_rel(Path::new(&mount), &rel), r.get::<_, i64>(0)?))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(rows)
}

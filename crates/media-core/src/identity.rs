//! Never read a known file twice.
//!
//! A file that shows up at a new path is checked against the index by content before anything is
//! read: same size + modification time, then the same 64 KiB prehash (one small read).
//!   * the original is gone (moved / renamed outside the app) → the existing row simply moves:
//!     same id, so analysis, faces, places, labels, date fixes, hash and thumbnails all stay;
//!   * the original is still there (a backup copy) → a new row takes over everything already
//!     derived from the original instead of decoding the file again.
//! A file edited while being moved gets a new mtime and is treated as new (safe default).

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use rusqlite::{params, Connection};

use crate::{db, journal, labels, meta, volumes};

/// Copy everything derived from `from` onto `to` (a byte-identical copy): analysis, place,
/// faces (same person, same confirmed state), the user's own labels. Thumbnails are picked up
/// lazily through `files.clone_of`.
pub fn clone_derived(conn: &Connection, from: i64, to: i64) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO analysis(file_id, version, sharpness, dhash, face_count, face_version, analyzed_at)
         SELECT ?2, version, sharpness, dhash, face_count, face_version, analyzed_at FROM analysis WHERE file_id = ?1",
        params![from, to],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO places(file_id, city, admin1, cc, source, dist_km)
         SELECT ?2, city, admin1, cc, source, dist_km FROM places WHERE file_id = ?1",
        params![from, to],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO labels(file_id, label, source) SELECT ?2, label, source FROM labels WHERE file_id = ?1 AND source = 'user'",
        params![from, to],
    )?;
    conn.execute("DELETE FROM faces WHERE file_id = ?", [to])?;
    conn.execute(
        "INSERT INTO faces(file_id, x, y, w, h, score, sharpness, model, embedding, person_id, locked, hidden, src_face)
         SELECT ?2, x, y, w, h, score, sharpness, model, embedding, person_id, locked, hidden, COALESCE(src_face, id)
         FROM faces WHERE file_id = ?1",
        params![from, to],
    )?;
    conn.execute("UPDATE files SET clone_of = ? WHERE id = ?", params![from, to])?;
    Ok(())
}

/// Recompute path-based labels (screenshot, messenger source, …) for a row at a new path,
/// from what is already indexed — no file read.
pub fn relabel_from_index(conn: &Connection, id: i64) -> Result<()> {
    let Some(f) = db::get_file(conn, id)? else { return Ok(()) };
    let m = meta::Meta {
        taken_at: f.taken_at,
        date_source: f.date_source.clone(),
        width: f.width,
        height: f.height,
        duration_ms: f.duration_ms,
        camera: f.camera.clone(),
        lat: f.lat,
        lon: f.lon,
        orientation: f.orientation,
    };
    let ls = labels::rule_labels(&f.rel_path, &f.name, &f.kind, &m);
    labels::replace_rule_labels(conn, id, &ls)
}

/// What to do with a file at a new path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Known {
    /// The indexed file `id` was moved/renamed here: update its path.
    Moved(i64),
    /// This is a copy of the indexed file `id`: reuse its results.
    CopyOf(i64),
}

/// Find an indexed file with the same content as the new file at `abs`, if any.
/// `gone` = ids already known to have left their old path in this scan; `taken` = ids that
/// were already claimed as moved by another new path (they can still be copy sources).
pub fn find_known(conn: &Connection, abs: &Path, size: i64, mtime: i64, gone: &HashSet<i64>, taken: &HashSet<i64>) -> Result<Option<Known>> {
    struct Donor {
        id: i64,
        prehash: Option<String>,
        missing: bool,
        path: std::path::PathBuf,
        online: bool,
    }
    let donors: Vec<Donor> = {
        let mut st = conn.prepare_cached(
            "SELECT f.id, f.prehash, f.missing, v.mount, f.rel_path, v.online FROM files f JOIN volumes v ON v.id = f.volume_id
             WHERE f.size = ? AND f.mtime = ? AND f.quarantined = 0",
        )?;
        let rows = st
            .query_map(params![size, mtime], |r| {
                let mount: String = r.get(3)?;
                let rel: String = r.get(4)?;
                Ok(Donor {
                    id: r.get(0)?,
                    prehash: r.get(1)?,
                    missing: r.get::<_, i64>(2)? != 0,
                    path: volumes::join_rel(Path::new(&mount), &rel),
                    online: r.get::<_, i64>(5)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    if donors.is_empty() {
        return Ok(None);
    }
    let Ok(mine) = journal::prehash_file(abs, size as u64) else { return Ok(None) };
    let mut copy_from = None;
    for d in donors {
        if d.path == abs {
            continue;
        }
        let exists = d.online && d.path.is_file();
        let ph = match d.prehash {
            Some(p) => Some(p),
            // Indexed before prehashes were stored: one small read of the original, kept.
            None if exists => {
                let p = journal::prehash_file(&d.path, size as u64).ok();
                if let Some(p) = &p {
                    conn.execute("UPDATE files SET prehash = ? WHERE id = ?", params![p, d.id])?;
                }
                p
            }
            None => None,
        };
        if ph.as_deref() != Some(mine.as_str()) {
            continue;
        }
        let left = gone.contains(&d.id) || (d.missing && !exists) || (d.online && !exists);
        if left && !taken.contains(&d.id) {
            return Ok(Some(Known::Moved(d.id)));
        }
        copy_from.get_or_insert(d.id);
    }
    Ok(copy_from.map(Known::CopyOf))
}

/// Point an existing row at its new location (moved/renamed outside the app).
pub fn apply_move(conn: &Connection, id: i64, volume_id: i64, rel: &str, name: &str, ext: &str, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE files SET volume_id = ?, rel_path = ?, name = ?, ext = ?, missing = 0, scanned_at = ? WHERE id = ?",
        params![volume_id, rel, name, ext, now, id],
    )?;
    relabel_from_index(conn, id)
}

/// New row for a copy of `from` at a new location, with everything derived from `from`.
pub fn apply_copy(conn: &Connection, from: i64, volume_id: i64, rel: &str, name: &str, ext: &str, now: i64) -> Result<i64> {
    let id: i64 = conn.query_row(
        "INSERT INTO files(volume_id, rel_path, name, ext, kind, size, mtime, taken_at, date_source, width, height,
                           duration_ms, camera, lat, lon, orientation, hash, prehash, missing, quarantined, scanned_at)
         SELECT ?1, ?2, ?3, ?4, kind, size, mtime, taken_at, date_source, width, height, duration_ms, camera, lat, lon,
                orientation, hash, prehash, 0, 0, ?5 FROM files WHERE id = ?6
         ON CONFLICT(volume_id, rel_path) DO UPDATE SET missing = 0, quarantined = 0
         RETURNING id",
        params![volume_id, rel, name, ext, now, from],
        |r| r.get(0),
    )?;
    clone_derived(conn, from, id)?;
    relabel_from_index(conn, id)?;
    Ok(id)
}

/// Files indexed before fingerprints were stored that share their size with another file (only
/// those can have a copy): fingerprint them once — one 64 KiB read each — so the backup check
/// and copy recognition see them. Connected drives only; returns how many were done.
pub fn backfill_prehashes(conn: &Connection) -> Result<usize> {
    use rayon::prelude::*;
    let todo: Vec<(i64, std::path::PathBuf, u64)> = {
        let mut st = conn.prepare(
            "SELECT f.id, v.mount, f.rel_path, f.size FROM files f JOIN volumes v ON v.id = f.volume_id
             WHERE f.prehash IS NULL AND f.missing = 0 AND f.quarantined = 0 AND v.online = 1
               AND EXISTS (SELECT 1 FROM files g WHERE g.id != f.id AND g.size = f.size AND g.missing = 0 AND g.quarantined = 0)",
        )?;
        let rows = st
            .query_map([], |r| {
                let mount: String = r.get(1)?;
                let rel: String = r.get(2)?;
                Ok((r.get(0)?, volumes::join_rel(Path::new(&mount), &rel), r.get::<_, i64>(3)? as u64))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let done: Vec<(i64, String)> = todo.par_iter().filter_map(|(id, p, size)| journal::prehash_file(p, *size).ok().map(|h| (*id, h))).collect();
    let mut st = conn.prepare("UPDATE files SET prehash = ? WHERE id = ?")?;
    for (id, h) in &done {
        st.execute(params![h, id])?;
    }
    Ok(done.len())
}

/// The user label that marks "this copy is the original".
pub const ORIGINAL_LABEL: &str = "original";

/// Rebuild which copy of each byte-identical file is the original and which are copies:
/// a copy the user marked as original wins, otherwise the duplicates screen's ranking (with the
/// user's keep rules). Reads nothing from disk; returns how many files have a role.
pub fn refresh_roles(conn: &mut Connection) -> Result<usize> {
    use std::collections::HashMap;
    let rules = crate::dedupe::DupRules::load(conn)?;
    let sql = format!(
        "SELECT {}, COALESCE(f.prehash, f.hash) k FROM files f JOIN volumes v ON v.id = f.volume_id
         WHERE f.missing = 0 AND f.quarantined = 0 AND COALESCE(f.prehash, f.hash) IN (
             SELECT COALESCE(prehash, hash) FROM files WHERE missing = 0 AND quarantined = 0 AND COALESCE(prehash, hash) IS NOT NULL
             GROUP BY COALESCE(prehash, hash) HAVING COUNT(*) > 1)",
        db::FILE_COLS
    );
    let mut groups: HashMap<String, Vec<db::FileRow>> = HashMap::new();
    {
        let mut st = conn.prepare(&sql)?;
        for r in st.query_map([], |r| Ok((db::FileRow::from_row(r)?, r.get::<_, String>(db::FILE_COLS_LEN)?)))? {
            let (f, k) = r?;
            groups.entry(k).or_default().push(f);
        }
    }
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM dup_roles", [])?;
    let mut n = 0;
    {
        let mut lab = tx.prepare("SELECT label FROM labels WHERE file_id = ?")?;
        let mut ins = tx.prepare("INSERT INTO dup_roles(file_id, role, group_size) VALUES (?, ?, ?)")?;
        for (_, mut g) in groups {
            let mut labels = HashMap::new();
            for f in &g {
                labels.insert(f.id, lab.query_map([f.id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?);
            }
            crate::dedupe::rank_members(&mut g, &labels, &rules);
            // The user's own pick goes first (stable: ranking order among the rest).
            if let Some(i) = g.iter().position(|f| labels[&f.id].iter().any(|l| l == ORIGINAL_LABEL)) {
                let picked = g.remove(i);
                g.insert(0, picked);
            }
            // Other drives: the best copy there is a backup; further copies anywhere are copies.
            let mut drives = std::collections::HashSet::new();
            for (i, f) in g.iter().enumerate() {
                let role = if i == 0 {
                    drives.insert(f.volume_id);
                    "original"
                } else if drives.insert(f.volume_id) {
                    "backup"
                } else {
                    "copy"
                };
                ins.execute(params![f.id, role, g.len() as i64])?;
                n += 1;
            }
        }
        // Resized / recompressed / edited versions of a similar photo, and the photo they are of.
        n += tx.execute(
            "INSERT OR IGNORE INTO dup_roles(file_id, role, group_size)
             SELECT m.file_id, CASE WHEN m.variant IS NULL THEN 'original' ELSE 'variant' END,
                    (SELECT COUNT(*) FROM dup_members m2 WHERE m2.group_id = m.group_id)
             FROM dup_members m JOIN dup_groups g ON g.id = m.group_id JOIN files f ON f.id = m.file_id
             WHERE g.kind = 'similar_image' AND g.resolved = 0 AND f.missing = 0 AND f.quarantined = 0",
            [],
        )?;
    }
    tx.commit()?;
    Ok(n)
}

/// "This copy is the original": the pick is remembered (one per group of identical files).
pub fn set_original(conn: &mut Connection, file_id: i64) -> Result<()> {
    let key: Option<String> = conn.query_row("SELECT COALESCE(prehash, hash) FROM files WHERE id = ?", [file_id], |r| r.get(0))?;
    {
        let tx = conn.transaction()?;
        if let Some(k) = key {
            tx.execute(
                "DELETE FROM labels WHERE label = ?1 AND file_id IN (SELECT id FROM files WHERE COALESCE(prehash, hash) = ?2)",
                params![ORIGINAL_LABEL, k],
            )?;
        }
        tx.execute("INSERT OR REPLACE INTO labels(file_id, label, source) VALUES (?, ?, 'user')", params![file_id, ORIGINAL_LABEL])?;
        tx.commit()?;
    }
    refresh_roles(conn)?;
    Ok(())
}

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
use rusqlite::{params, Connection, OptionalExtension};

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
    let stop = crate::StopFlag::new();
    let mut done = 0;
    loop {
        let n = backfill_prehashes_with(conn, 5000, &stop, &|| false)?;
        done += n;
        if n == 0 {
            return Ok(done);
        }
    }
}

/// Up to `limit` missing fingerprints, read in folder order with two reads at a time (a hard
/// disk is slower with more), checking `stop` / `paused` between small chunks. A file that cannot
/// be read gets a fingerprint of its own (`unreadable-<id>`): not read again, and never taken for
/// a copy of anything.
pub fn backfill_prehashes_with(conn: &Connection, limit: usize, stop: &crate::StopFlag, paused: &dyn Fn() -> bool) -> Result<usize> {
    backfill_where(conn, "", limit, stop, paused)
}

/// Only the files that could be a copy of something on drive `volume_id` (same size as one of
/// its files) — all an import from a card needs, instead of fingerprinting the whole library.
pub fn backfill_prehashes_for_volume(conn: &Connection, volume_id: i64, stop: &crate::StopFlag) -> Result<usize> {
    let only = format!("AND f.size IN (SELECT size FROM files WHERE volume_id = {volume_id} AND missing = 0 AND quarantined = 0)");
    let mut done = 0;
    loop {
        let n = backfill_where(conn, &only, 2000, stop, &|| false)?;
        done += n;
        if n == 0 || stop.is_stopped() {
            return Ok(done);
        }
    }
}

fn backfill_where(conn: &Connection, only: &str, limit: usize, stop: &crate::StopFlag, paused: &dyn Fn() -> bool) -> Result<usize> {
    use rayon::prelude::*;
    let todo: Vec<(i64, std::path::PathBuf, u64)> = {
        let mut st = conn.prepare(&format!(
            "SELECT f.id, v.mount, f.rel_path, f.size FROM files f JOIN volumes v ON v.id = f.volume_id
             WHERE f.prehash IS NULL AND f.missing = 0 AND f.quarantined = 0 AND v.online = 1
               AND EXISTS (SELECT 1 FROM files g WHERE g.id != f.id AND g.size = f.size AND g.missing = 0 AND g.quarantined = 0)
               {only}
             ORDER BY f.volume_id, f.rel_path LIMIT ?"
        ))?;
        let rows = st
            .query_map([limit.min(i64::MAX as usize) as i64], |r| {
                let mount: String = r.get(1)?;
                let rel: String = r.get(2)?;
                Ok((r.get(0)?, volumes::join_rel(Path::new(&mount), &rel), r.get::<_, i64>(3)? as u64))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    if todo.is_empty() {
        return Ok(0);
    }
    let pool = rayon::ThreadPoolBuilder::new().num_threads(2).build()?;
    let mut st = conn.prepare("UPDATE files SET prehash = ? WHERE id = ?")?;
    let mut done = 0;
    for chunk in todo.chunks(64) {
        if stop.is_stopped() || paused() {
            break;
        }
        let hashes: Vec<(i64, String)> =
            pool.install(|| chunk.par_iter().map(|(id, p, size)| (*id, journal::prehash_file(p, *size).unwrap_or_else(|_| format!("unreadable-{id}")))).collect());
        // A transaction that is always closed (an error inside must not leave it open on this
        // connection).
        let tx = conn.unchecked_transaction()?;
        for (id, h) in &hashes {
            st.execute(params![h, id])?;
        }
        tx.commit()?;
        done += hashes.len();
    }
    Ok(done)
}

/// One of the files that are the same photo.
#[derive(Clone, Debug, serde::Serialize)]
pub struct FamilyMember {
    pub file: db::FileRow,
    pub abs_path: String,
    /// "original" | "backup" | "copy" | "variant"
    pub role: String,
    /// In words: 원본 / 복사본 (same name) / 사본 (another name) / 백업 (another drive) /
    /// 축소본 / 재압축본 / 편집본.
    pub relation: String,
    pub is_self: bool,
}

/// A file with everything that is the same photo: where the original is, what the others are,
/// and why that one is the original.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Family {
    /// "exact" (identical files) or "similar" (versions of one photo).
    pub kind: String,
    pub why: String,
    /// The original first.
    pub members: Vec<FamilyMember>,
}

/// The files that are the same photo as `id` — identical ones if there are any, otherwise the
/// versions of its similar-photo group. None when the file stands alone.
pub fn family(conn: &Connection, id: i64) -> Result<Option<Family>> {
    let Some(me) = db::get_file(conn, id)? else { return Ok(None) };
    let member = |f: db::FileRow, role: &str, relation: &str| FamilyMember {
        abs_path: f.abs_path().to_string_lossy().into_owned(),
        is_self: f.id == id,
        role: role.into(),
        relation: relation.into(),
        file: f,
    };
    let key: Option<String> = conn.query_row("SELECT COALESCE(prehash, hash) FROM files WHERE id = ?", [id], |r| r.get(0))?;
    if let Some(key) = key {
        let sql = format!(
            "SELECT {}, r.role FROM files f JOIN volumes v ON v.id = f.volume_id LEFT JOIN dup_roles r ON r.file_id = f.id
             WHERE COALESCE(f.prehash, f.hash) = ?1 AND f.size = ?2 AND f.missing = 0 AND f.quarantined = 0",
            db::FILE_COLS
        );
        let mut rows: Vec<(db::FileRow, Option<String>)> = {
            let mut st = conn.prepare(&sql)?;
            let v = st.query_map(params![key, me.size], |r| Ok((db::FileRow::from_row(r)?, r.get(db::FILE_COLS_LEN)?)))?.collect::<rusqlite::Result<_>>()?;
            v
        };
        if rows.len() > 1 {
            let rank = |r: &Option<String>| match r.as_deref() {
                Some("original") => 0,
                Some("backup") => 1,
                _ => 2,
            };
            rows.sort_by(|a, b| rank(&a.1).cmp(&rank(&b.1)).then(a.0.id.cmp(&b.0.id)));
            let first = rows[0].0.clone();
            let rest: Vec<db::FileRow> = rows[1..].iter().map(|r| r.0.clone()).collect();
            let marked: bool = conn.query_row("SELECT 1 FROM labels WHERE file_id = ? AND label = ?", params![first.id, ORIGINAL_LABEL], |_| Ok(())).is_ok();
            let lib = crate::library::location(conn);
            let rules = crate::dedupe::DupRules::load(conn)?;
            let why = crate::library::why_original(&first, &rest, marked, lib.as_ref().map(|(v, r)| (*v, r.as_str())), &rules);
            let members = rows
                .into_iter()
                .enumerate()
                .map(|(i, (f, role))| {
                    let (role, relation) = if i == 0 {
                        ("original", "원본")
                    } else if role.as_deref() == Some("backup") || f.volume_id != first.volume_id {
                        ("backup", "백업")
                    } else if f.name.to_lowercase() == first.name.to_lowercase() {
                        ("copy", "복사본")
                    } else {
                        ("copy", "사본")
                    };
                    member(f, role, relation)
                })
                .collect();
            return Ok(Some(Family { kind: "exact".into(), why, members }));
        }
    }
    // Versions of one photo (similar-photo group).
    let gid: Option<i64> = conn
        .query_row(
            "SELECT m.group_id FROM dup_members m JOIN dup_groups g ON g.id = m.group_id
             WHERE m.file_id = ? AND g.kind = 'similar_image' AND g.resolved = 0",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(gid) = gid else { return Ok(None) };
    let sql = format!(
        "SELECT {}, m.variant FROM dup_members m JOIN files f ON f.id = m.file_id JOIN volumes v ON v.id = f.volume_id
         WHERE m.group_id = ? AND f.missing = 0 AND f.quarantined = 0 ORDER BY m.variant IS NOT NULL, m.rank, f.id",
        db::FILE_COLS
    );
    let mut st = conn.prepare(&sql)?;
    let members: Vec<FamilyMember> = st
        .query_map([gid], |r| Ok((db::FileRow::from_row(r)?, r.get::<_, Option<String>>(db::FILE_COLS_LEN)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(f, v)| match v.as_deref() {
            None => member(f, "original", "원본"),
            Some("smaller") => member(f, "variant", "축소본"),
            Some("recompressed") => member(f, "variant", "재압축본"),
            Some(_) => member(f, "variant", "편집본"),
        })
        .collect();
    if members.len() < 2 {
        return Ok(None);
    }
    Ok(Some(Family { kind: "similar".into(), why: "가장 크고 선명한 버전".into(), members }))
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
    let lib = crate::library::location(conn);
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
            // The same choice as the organizer makes (library drive, filed, named folder), so
            // "원본" means one thing everywhere.
            if let Some((vol, root)) = &lib {
                crate::library::keeper_first(&mut g, *vol, root);
            }
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

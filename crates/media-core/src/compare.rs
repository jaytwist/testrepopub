//! Side-by-side comparison of a few files: which ones are byte-identical, and which one to keep.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::{db, dedupe, journal};

#[derive(Clone, Debug, Default, Serialize)]
pub struct Comparison {
    /// Groups (2+ ids) of files with exactly the same content.
    pub identical: Vec<Vec<i64>>,
    /// The one to keep, by the same rules as the duplicates screen.
    pub recommended: Option<i64>,
}

/// Compare files. Only same-size files can be identical; their stored full hash is used, and a
/// missing one is computed once (connected drives only) and stored, so it is never read again.
pub fn compare(conn: &Connection, ids: &[i64]) -> Result<Comparison> {
    let mut files = Vec::new();
    for id in ids {
        if let Some(f) = db::get_file(conn, *id)? {
            files.push(f);
        }
    }
    let mut by_size: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        by_size.entry(f.size).or_default().push(i);
    }
    let mut identical = Vec::new();
    for idx in by_size.values().filter(|v| v.len() > 1) {
        let mut by_hash: HashMap<String, Vec<i64>> = HashMap::new();
        for &i in idx {
            let f = &files[i];
            let h = match &f.hash {
                Some(h) => Some(h.clone()),
                None if f.online => match journal::hash_file(&f.abs_path()) {
                    Ok(h) => {
                        conn.execute("UPDATE files SET hash = ? WHERE id = ?", params![h, f.id])?;
                        Some(h)
                    }
                    Err(_) => None,
                },
                None => None,
            };
            if let Some(h) = h {
                by_hash.entry(h).or_default().push(f.id);
            }
        }
        identical.extend(by_hash.into_values().filter(|g| g.len() > 1));
    }
    for g in &mut identical {
        g.sort();
    }
    identical.sort();

    let mut labels: HashMap<i64, Vec<String>> = HashMap::new();
    {
        let mut st = conn.prepare("SELECT label FROM labels WHERE file_id = ?")?;
        for f in &files {
            labels.insert(f.id, st.query_map([f.id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?);
        }
    }
    let mut ranked = files.clone();
    dedupe::rank_members(&mut ranked, &labels);
    Ok(Comparison { identical, recommended: ranked.first().map(|f| f.id) })
}

/// Photos that look like this one (resized, recompressed, lightly edited copies), most similar
/// first — from the 64-bit picture fingerprints computed at analysis, so nothing is read.
pub fn similar_to(conn: &Connection, id: i64, limit: usize) -> Result<Vec<i64>> {
    /// Resized / recompressed copies land at 0–2 of 64 bits (measured: a messenger copy of a
    /// 12 MP photo scored 0); different photos of a similar scene start around 4.
    const MAX_DIST: u32 = 3;
    /// A resized copy keeps its shape; crops and screenshots of it do not count as the same photo.
    const MAX_ASPECT_DIFF: f64 = 0.03;
    let Some((h, w0, h0)) = conn
        .query_row(
            "SELECT a.dhash, f.width, f.height FROM analysis a JOIN files f ON f.id = a.file_id WHERE a.file_id = ? AND a.dhash IS NOT NULL",
            [id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<i64>>(2)?)),
        )
        .ok()
    else {
        return Ok(Vec::new());
    };
    let aspect = |w: Option<i64>, h: Option<i64>| match (w, h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some(w as f64 / h as f64),
        _ => None,
    };
    let a0 = aspect(w0, h0);
    let mut st = conn.prepare(
        "SELECT a.file_id, a.dhash, f.width, f.height FROM analysis a JOIN files f ON f.id = a.file_id
         WHERE a.dhash IS NOT NULL AND a.file_id != ? AND f.missing = 0 AND f.quarantined = 0",
    )?;
    let mut hits: Vec<(u32, i64)> = st
        .query_map([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<i64>>(3)?)))?
        .filter_map(|r| r.ok())
        .filter(|(_, _, w, hh)| match (a0, aspect(*w, *hh)) {
            (Some(x), Some(y)) => (x - y).abs() / x <= MAX_ASPECT_DIFF,
            _ => true,
        })
        .map(|(fid, d, _, _)| (crate::analysis::hamming(h as u64, d as u64), fid))
        .filter(|(d, _)| *d <= MAX_DIST)
        .collect();
    hits.sort();
    Ok(hits.into_iter().take(limit).map(|(_, fid)| fid).collect())
}

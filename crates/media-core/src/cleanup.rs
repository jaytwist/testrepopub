//! Cleanup suggestions, recomputed from the per-file analysis after every analysis pass:
//!   * bursts      — shots seconds apart that look alike; the best one is kept, the rest suggested
//!   * blurry      — focus far below what the same day's photos achieve (relative, so plain skies
//!                   and smooth images are not flagged just for being smooth)
//! Screenshots and large videos come straight from labels / sizes. Anything the user marks as
//! "keep" is remembered in `dismissals` and never suggested again.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::analysis::hamming;

/// Shots at most this many seconds apart can be one burst.
const BURST_GAP_SECS: i64 = 3;
/// Max dHash distance between consecutive shots of a burst.
const BURST_HASH_DIST: u32 = 12;
/// Absolute focus score below which a photo can be called blurry (1024 px Laplacian, calibrated on
/// real photos: sharp 280–670, visibly blurred 10–55).
const BLUR_ABS: f64 = 40.0;
/// …and it must also be below this fraction of the comparison median.
const BLUR_REL: f64 = 0.25;
/// Videos at least this big are offered in "large videos".
pub const LARGE_VIDEO_BYTES: i64 = 500 * 1024 * 1024;

struct Shot {
    id: i64,
    taken: i64,
    camera: String,
    dhash: u64,
    sharpness: f64,
    pixels: i64,
    size: i64,
    faces: i64,
}

/// Rebuild bursts + burst/blur suggestions. Cheap (pure SQL + memory), runs after each analysis pass.
pub fn refresh(conn: &mut Connection) -> Result<()> {
    let shots: Vec<Shot> = {
        let mut st = conn.prepare(
            "SELECT f.id, f.taken_at, COALESCE(f.camera, ''), a.dhash, COALESCE(a.sharpness, 0),
                    COALESCE(f.width, 0) * COALESCE(f.height, 0), f.size, COALESCE(a.face_count, 0)
             FROM files f JOIN analysis a ON a.file_id = f.id
             WHERE f.missing = 0 AND f.quarantined = 0 AND f.kind IN ('photo', 'raw') AND f.date_source = 'exif'
               AND a.dhash IS NOT NULL AND f.taken_at IS NOT NULL
             ORDER BY COALESCE(f.camera, ''), f.taken_at, f.id",
        )?;
        let rows = st
            .query_map([], |r| {
                Ok(Shot {
                    id: r.get(0)?,
                    taken: r.get(1)?,
                    camera: r.get(2)?,
                    dhash: r.get::<_, i64>(3)? as u64,
                    sharpness: r.get(4)?,
                    pixels: r.get(5)?,
                    size: r.get(6)?,
                    faces: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    // --- bursts ---
    let mut groups: Vec<Vec<&Shot>> = Vec::new();
    let mut cur: Vec<&Shot> = Vec::new();
    for s in &shots {
        // Same time + same size + same look = a copy of the same file (the duplicate tool's job).
        if cur.last().is_some_and(|p| p.taken == s.taken && p.size == s.size && p.dhash == s.dhash) {
            continue;
        }
        // Joins the current burst: same camera, seconds after the previous shot, and looks like at
        // least one shot already in the burst (a single shaken frame doesn't split the burst).
        let joins = cur.last().is_some_and(|p| p.camera == s.camera && s.taken - p.taken <= BURST_GAP_SECS)
            && cur.iter().any(|q| hamming(q.dhash, s.dhash) <= BURST_HASH_DIST);
        if !joins {
            if cur.len() > 1 {
                groups.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        }
        cur.push(s);
    }
    if cur.len() > 1 {
        groups.push(cur);
    }

    // --- blurry ---
    let day = |t: i64| t.div_euclid(86_400);
    let month = |t: i64| t.div_euclid(86_400 * 30);
    let median = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.total_cmp(b));
        v[v.len() / 2]
    };
    let mut by_day: HashMap<i64, Vec<f64>> = HashMap::new();
    let mut by_month: HashMap<i64, Vec<f64>> = HashMap::new();
    for s in &shots {
        by_day.entry(day(s.taken)).or_default().push(s.sharpness);
        by_month.entry(month(s.taken)).or_default().push(s.sharpness);
    }
    let mut all: Vec<f64> = shots.iter().map(|s| s.sharpness).collect();
    let global = if all.is_empty() { 0.0 } else { median(&mut all) };
    let day_med: HashMap<i64, f64> = by_day.into_iter().filter(|(_, v)| v.len() >= 5).map(|(k, mut v)| (k, median(&mut v))).collect();
    let month_med: HashMap<i64, f64> = by_month.into_iter().filter(|(_, v)| v.len() >= 5).map(|(k, mut v)| (k, median(&mut v))).collect();

    let tx = conn.transaction()?;
    tx.execute("DELETE FROM bursts", [])?;
    tx.execute("DELETE FROM suggestions WHERE kind IN ('burst_extra', 'blurry')", [])?;
    {
        let mut ins_b = tx.prepare("INSERT INTO bursts(file_id, burst_id, rank) VALUES (?, ?, ?)")?;
        let mut ins_s = tx.prepare("INSERT OR REPLACE INTO suggestions(file_id, kind, detail) VALUES (?, ?, ?)")?;
        for (gi, g) in groups.iter().enumerate() {
            let mut ranked: Vec<&&Shot> = g.iter().collect();
            // Best shot: sharpest, then most pixels, then more faces.
            ranked.sort_by(|a, b| b.sharpness.total_cmp(&a.sharpness).then(b.pixels.cmp(&a.pixels)).then(b.faces.cmp(&a.faces)));
            let bid = gi as i64 + 1;
            for (rank, s) in ranked.iter().enumerate() {
                ins_b.execute(params![s.id, bid, rank as i64])?;
                if rank > 0 {
                    ins_s.execute(params![s.id, "burst_extra", format!("연사 {}장 중 하나 · 가장 선명한 사진이 따로 있음", g.len())])?;
                }
            }
        }
        for s in &shots {
            let (reference, scope) = match (day_med.get(&day(s.taken)), month_med.get(&month(s.taken))) {
                (Some(m), _) => (*m, "같은 날"),
                (None, Some(m)) => (*m, "같은 달"),
                _ => (global, "전체"),
            };
            if s.sharpness < BLUR_ABS && s.sharpness < BLUR_REL * reference {
                ins_s.execute(params![s.id, "blurry", format!("선명도 {:.0} · {scope} 사진 보통 {:.0}", s.sharpness, reference)])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Summary for the Cleanup screen
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize)]
pub struct Bucket {
    pub count: i64,
    pub bytes: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CleanupSummary {
    pub blurry: Bucket,
    /// count = extra shots (all but the best), groups = number of bursts
    pub burst_extras: Bucket,
    pub burst_groups: i64,
    pub screenshots: Bucket,
    pub large_videos: Bucket,
    pub exact_duplicates: Bucket,
    /// Everything above (duplicates counted once).
    pub total_bytes: i64,
    pub analyzed: i64,
    pub pending: i64,
}

fn bucket(conn: &Connection, sql: &str) -> Result<Bucket> {
    Ok(conn.query_row(sql, [], |r| Ok(Bucket { count: r.get(0)?, bytes: r.get(1)? }))?)
}

pub fn summary(conn: &Connection) -> Result<CleanupSummary> {
    let live = "f.missing = 0 AND f.quarantined = 0";
    let sugg = |kind: &str| {
        format!(
            "SELECT COUNT(*), COALESCE(SUM(f.size), 0) FROM files f JOIN suggestions s ON s.file_id = f.id AND s.kind = '{kind}'
             WHERE {live} AND NOT EXISTS (SELECT 1 FROM dismissals d WHERE d.file_id = f.id AND d.kind = '{kind}')"
        )
    };
    let blurry = bucket(conn, &sugg("blurry"))?;
    let burst_extras = bucket(conn, &sugg("burst_extra"))?;
    let burst_groups: i64 = conn.query_row(
        &format!("SELECT COUNT(DISTINCT b.burst_id) FROM bursts b JOIN files f ON f.id = b.file_id WHERE {live}"),
        [],
        |r| r.get(0),
    )?;
    let screenshots = bucket(
        conn,
        &format!(
            "SELECT COUNT(*), COALESCE(SUM(f.size), 0) FROM files f WHERE {live}
               AND EXISTS (SELECT 1 FROM labels l WHERE l.file_id = f.id AND l.label = 'screenshot')
               AND NOT EXISTS (SELECT 1 FROM dismissals d WHERE d.file_id = f.id AND d.kind = 'screenshot')"
        ),
    )?;
    let large_videos = bucket(
        conn,
        &format!(
            "SELECT COUNT(*), COALESCE(SUM(f.size), 0) FROM files f WHERE {live} AND f.kind = 'video' AND f.size >= {LARGE_VIDEO_BYTES}
               AND NOT EXISTS (SELECT 1 FROM dismissals d WHERE d.file_id = f.id AND d.kind = 'large_video')"
        ),
    )?;
    let exact_duplicates = crate::dedupe::summary(conn)?
        .into_iter()
        .find(|s| s.kind == "exact")
        .map(|s| Bucket { count: s.files - s.groups, bytes: s.reclaimable_bytes })
        .unwrap_or_default();
    let (analyzed, pending): (i64, i64) = conn.query_row(
        &format!(
            "SELECT COALESCE(SUM(a.file_id IS NOT NULL), 0), COALESCE(SUM(a.file_id IS NULL), 0)
             FROM files f LEFT JOIN analysis a ON a.file_id = f.id WHERE {live}"
        ),
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let total_bytes = blurry.bytes + burst_extras.bytes + screenshots.bytes + large_videos.bytes + exact_duplicates.bytes;
    Ok(CleanupSummary { blurry, burst_extras, burst_groups, screenshots, large_videos, exact_duplicates, total_bytes, analyzed, pending })
}

/// Ids of burst shots that are not the best one (to pre-select for review).
pub fn burst_extra_ids(conn: &Connection) -> Result<Vec<i64>> {
    let mut st = conn.prepare(
        "SELECT b.file_id FROM bursts b JOIN files f ON f.id = b.file_id
         WHERE b.rank > 0 AND f.missing = 0 AND f.quarantined = 0
           AND NOT EXISTS (SELECT 1 FROM dismissals d WHERE d.file_id = f.id AND d.kind = 'burst_extra')",
    )?;
    let rows = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(rows)
}

/// "Keep this one": never suggest these files for `kind` again.
pub fn dismiss(conn: &mut Connection, ids: &[i64], kind: &str) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut n = 0;
    {
        let mut st = tx.prepare("INSERT OR IGNORE INTO dismissals(file_id, kind) VALUES (?, ?)")?;
        for id in ids {
            n += st.execute(params![id, kind])?;
        }
    }
    tx.commit()?;
    Ok(n)
}

/// Detail text for a file's suggestions (shown in the viewer: why was this suggested?).
pub fn reasons(conn: &Connection, file_id: i64) -> Result<Vec<(String, String)>> {
    let mut st = conn.prepare(
        "SELECT s.kind, s.detail FROM suggestions s WHERE s.file_id = ?
           AND NOT EXISTS (SELECT 1 FROM dismissals d WHERE d.file_id = s.file_id AND d.kind = s.kind)",
    )?;
    let rows = st.query_map([file_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

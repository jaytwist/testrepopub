//! Quick review ("빠른 검토"): groups of related files with a suggested decision (which to keep),
//! and saving what the user decided. Nothing is deleted here: the UI removes the dropped files
//! through the normal protected delete (`journal::delete_files`), so originals stay safe.
//!
//! Group keys: `dup:<dup_groups.id>` (exact copies, similar photos / videos) and
//! `burst:<bursts.burst_id>`.

use std::collections::HashSet;

use anyhow::{bail, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::db::{FileRow, FILE_COLS, FILE_COLS_LEN};

#[derive(Clone, Debug, Serialize)]
pub struct TriageMember {
    pub file: FileRow,
    /// Suggested (or previously decided): keep this one.
    pub keep: bool,
    /// "backup": the copy on another drive — always kept (`locked`).
    pub role: Option<String>,
    /// Similar photos: "smaller" | "recompressed" | "edited".
    pub variant: Option<String>,
    pub sharpness: Option<f64>,
    /// Cannot be marked for removal here.
    pub locked: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct TriageGroup {
    pub key: String,
    pub members: Vec<TriageMember>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Decision {
    pub key: String,
    /// Files of the group to keep; the rest will be removed by the caller.
    pub keep: Vec<i64>,
}

const LIVE: &str = "f.missing = 0 AND f.quarantined = 0";

/// Groups still to review. `kind`: "burst" | "exact" | "similar_image" | "similar_video".
/// `variant` narrows similar photos: "safe" (resized / recompressed copies) or "edited".
pub fn groups(conn: &Connection, kind: &str, variant: Option<&str>, offset: i64, limit: i64) -> Result<Vec<TriageGroup>> {
    match kind {
        "burst" => bursts(conn, offset, limit),
        "exact" | "similar_image" | "similar_video" => dups(conn, kind, variant, offset, limit),
        _ => bail!("unknown review kind {kind}"),
    }
}

fn bursts(conn: &Connection, offset: i64, limit: i64) -> Result<Vec<TriageGroup>> {
    // Bursts with at least one extra shot not yet kept by the user.
    let ids: Vec<i64> = {
        let mut st = conn.prepare(&format!(
            "SELECT b.burst_id FROM bursts b JOIN files f ON f.id = b.file_id
             WHERE {LIVE}
             GROUP BY b.burst_id
             HAVING COUNT(*) > 1 AND SUM(EXISTS (SELECT 1 FROM suggestions s WHERE s.file_id = b.file_id AND s.kind = 'burst_extra')
                                     AND NOT EXISTS (SELECT 1 FROM dismissals d WHERE d.file_id = b.file_id AND d.kind = 'burst_extra')) > 0
             ORDER BY MIN(f.taken_at) DESC, b.burst_id
             LIMIT ? OFFSET ?"
        ))?;
        let rows = st.query_map(params![limit, offset], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    let mut st = conn.prepare(&format!(
        "SELECT {FILE_COLS}, a.sharpness,
                EXISTS (SELECT 1 FROM suggestions s WHERE s.file_id = f.id AND s.kind = 'burst_extra')
                AND NOT EXISTS (SELECT 1 FROM dismissals d WHERE d.file_id = f.id AND d.kind = 'burst_extra')
         FROM bursts b JOIN files f ON f.id = b.file_id JOIN volumes v ON v.id = f.volume_id
         LEFT JOIN analysis a ON a.file_id = f.id
         WHERE b.burst_id = ? AND {LIVE} ORDER BY b.rank, f.id"
    ))?;
    let mut out = Vec::new();
    for id in ids {
        let members = st
            .query_map([id], |r| {
                let extra: bool = r.get::<_, i64>(FILE_COLS_LEN + 1)? != 0;
                Ok(TriageMember { file: FileRow::from_row(r)?, keep: !extra, role: None, variant: None, sharpness: r.get(FILE_COLS_LEN)?, locked: false })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out.push(TriageGroup { key: format!("burst:{id}"), members });
    }
    Ok(out)
}

fn dups(conn: &Connection, kind: &str, variant: Option<&str>, offset: i64, limit: i64) -> Result<Vec<TriageGroup>> {
    let narrow = match variant {
        Some("safe") => "AND EXISTS (SELECT 1 FROM dup_members m2 WHERE m2.group_id = g.id AND m2.variant IN ('smaller', 'recompressed'))",
        Some("edited") => "AND EXISTS (SELECT 1 FROM dup_members m2 WHERE m2.group_id = g.id AND m2.variant = 'edited')",
        _ => "",
    };
    let ids: Vec<i64> = {
        let mut st = conn.prepare(&format!(
            "SELECT g.id FROM dup_groups g WHERE g.kind = ? AND g.resolved = 0 {narrow}
               AND (SELECT COUNT(*) FROM dup_members m JOIN files f ON f.id = m.file_id WHERE m.group_id = g.id AND {LIVE}) > 1
             ORDER BY (SELECT SUM(f.size) FROM dup_members m JOIN files f ON f.id = m.file_id WHERE m.group_id = g.id AND m.keep = 0) DESC, g.id
             LIMIT ? OFFSET ?"
        ))?;
        let rows = st.query_map(params![kind, limit, offset], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    let mut st = conn.prepare(&format!(
        "SELECT {FILE_COLS}, a.sharpness, m.keep, m.variant
         FROM dup_members m JOIN files f ON f.id = m.file_id JOIN volumes v ON v.id = f.volume_id
         LEFT JOIN analysis a ON a.file_id = f.id
         WHERE m.group_id = ? AND {LIVE} ORDER BY m.rank, f.id"
    ))?;
    let mut out = Vec::new();
    for id in ids {
        let mut members = st
            .query_map([id], |r| {
                Ok(TriageMember {
                    file: FileRow::from_row(r)?,
                    sharpness: r.get(FILE_COLS_LEN)?,
                    keep: r.get::<_, i64>(FILE_COLS_LEN + 1)? != 0,
                    variant: r.get(FILE_COLS_LEN + 2)?,
                    role: None,
                    locked: false,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if kind == "exact" {
            // The best copy on each other drive is a backup: it stays, whatever is chosen here.
            let first_drive = members.first().map(|m| m.file.volume_id);
            let mut seen = HashSet::new();
            for m in &mut members {
                if seen.insert(m.file.volume_id) && Some(m.file.volume_id) != first_drive {
                    m.role = Some("backup".into());
                    m.locked = true;
                    m.keep = true;
                }
            }
        }
        out.push(TriageGroup { key: format!("dup:{id}"), members });
    }
    Ok(out)
}

/// Save decisions: duplicate groups remember which members are kept (a group where everything is
/// kept is done); burst shots the user kept are not suggested again.
pub fn commit(conn: &mut Connection, decisions: &[Decision]) -> Result<()> {
    let tx = conn.transaction()?;
    for d in decisions {
        let keep: HashSet<i64> = d.keep.iter().copied().collect();
        if let Some(id) = d.key.strip_prefix("dup:") {
            let gid: i64 = id.parse()?;
            let members: Vec<i64> = {
                let mut st = tx.prepare(&format!("SELECT m.file_id FROM dup_members m JOIN files f ON f.id = m.file_id WHERE m.group_id = ? AND {LIVE}"))?;
                let rows = st.query_map([gid], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
                rows
            };
            for f in &members {
                tx.execute("UPDATE dup_members SET keep = ? WHERE group_id = ? AND file_id = ?", params![keep.contains(f) as i64, gid, f])?;
            }
            if members.iter().all(|f| keep.contains(f)) {
                tx.execute("UPDATE dup_groups SET resolved = 1 WHERE id = ?", [gid])?;
            }
        } else if let Some(id) = d.key.strip_prefix("burst:") {
            let bid: i64 = id.parse()?;
            let mut st = tx.prepare("SELECT file_id FROM bursts WHERE burst_id = ?")?;
            let members: Vec<i64> = st.query_map([bid], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            for f in members.iter().filter(|f| keep.contains(f)) {
                tx.execute("INSERT OR IGNORE INTO dismissals(file_id, kind) VALUES (?, 'burst_extra')", [f])?;
            }
        } else {
            bail!("unknown review group {}", d.key);
        }
    }
    tx.commit()?;
    Ok(())
}

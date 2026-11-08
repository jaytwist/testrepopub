//! Protection: files that are not deleted unless the user insists (`force`). Every delete path
//! (selection, compare, review, duplicate apply) goes through this one check.
//!
//! * `last_copy` — every live copy of the same content is being deleted at once; one is kept
//!   (content = the 64 KiB prehash, else the full hash — the same key as `identity`).
//!   A file with no known copy (a lone screenshot) is not protected by this.
//! * `keep_folder` — inside a folder the user marked "always keep" (duplicate rules).
//! * `original` — the user marked it as the original of its copies.
//! * `favorite` — marked ♥.
//!
//! Nothing here reads files: content identity is the stored hash / 64 KiB prehash.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::db;
use crate::dedupe::DupRules;
use crate::identity::ORIGINAL_LABEL;

pub const FAVORITE_LABEL: &str = "favorite";

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Blocked {
    pub id: i64,
    /// "last_copy" | "keep_folder" | "original" | "favorite"
    pub reason: String,
}

/// Which of `ids` are protected, and why (one reason per file, most important first).
pub fn check(conn: &Connection, ids: &[i64]) -> Result<Vec<Blocked>> {
    let rules = DupRules::load(conn)?;
    let wanted: HashSet<i64> = ids.iter().copied().collect();
    let mut out: Vec<Blocked> = Vec::new();
    let mut by_content: HashMap<String, Vec<i64>> = HashMap::new();
    let mut label_st = conn.prepare_cached("SELECT label FROM labels WHERE file_id = ? AND label IN (?, ?)")?;
    let mut key_st = conn.prepare_cached("SELECT COALESCE(prehash, hash) FROM files WHERE id = ?")?;
    for &id in &wanted {
        let Some(f) = db::get_file(conn, id)? else { continue };
        if f.quarantined || f.missing {
            continue;
        }
        let labels: Vec<String> =
            label_st.query_map(params![id, ORIGINAL_LABEL, FAVORITE_LABEL], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let reason = if labels.iter().any(|l| l == ORIGINAL_LABEL) {
            Some("original")
        } else if !labels.is_empty() {
            Some("favorite")
        } else if rules.is_kept(&f) {
            Some("keep_folder")
        } else {
            None
        };
        if let Some(r) = reason {
            out.push(Blocked { id, reason: r.into() });
            continue;
        }
        let key: Option<String> = key_st.query_row([id], |r| r.get(0))?;
        if let Some(key) = key {
            by_content.entry(key).or_default().push(id);
        }
    }
    // Deleting every live copy of some content: keep one (the marked original if it is there).
    // `files_content` indexes exactly this expression.
    let mut count = conn.prepare_cached(
        "SELECT COUNT(*) FROM files WHERE COALESCE(prehash, hash) = ?1 AND missing = 0 AND quarantined = 0",
    )?;
    let mut role = conn.prepare_cached("SELECT role FROM dup_roles WHERE file_id = ?")?;
    for (key, mut members) in by_content {
        let live: i64 = count.query_row([&key], |r| r.get(0))?;
        // A copy outside the request (or protected above, so kept anyway) survives: fine.
        if live < 2 || (live as usize) > members.len() {
            continue;
        }
        members.sort();
        let mut keep = members[0];
        for &m in &members {
            if role.query_row([m], |r| r.get::<_, String>(0)).optional()?.as_deref() == Some("original") {
                keep = m;
                break;
            }
        }
        out.push(Blocked { id: keep, reason: "last_copy".into() });
    }
    out.sort_by_key(|b| b.id);
    Ok(out)
}

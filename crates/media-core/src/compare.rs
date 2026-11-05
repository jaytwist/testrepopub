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

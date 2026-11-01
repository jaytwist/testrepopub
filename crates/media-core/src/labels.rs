//! Labels. Two layers:
//!  * `rule` labels: derived on every scan from name, path and metadata (screenshot, messenger,
//!    camera, GPS, date confidence). Regenerated, never edited by hand.
//!  * `user` labels: free text the user adds; never touched by scans.
//! (`ml` labels from on-device CLIP are planned for a later phase and use the same table.)

use std::collections::BTreeSet;

use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::meta::Meta;

static RE_SCREENSHOT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)screenshot|screen[ _-]?shot|스크린샷|화면[ _]?캡처|캡처|screen[ _-]?recording|화면[ _]?녹화|snip").unwrap());
static RE_KAKAO: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)kakaotalk|카카오톡").unwrap());
static RE_WHATSAPP: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)-WA\d{4}|whatsapp").unwrap());
static RE_TELEGRAM: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)telegram").unwrap());
static RE_LINE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?:^|/)LINE(?:/|$)|(?i)line_\d{10,}").unwrap());
static RE_INSTAGRAM: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)instagram").unwrap());
static RE_DOWNLOAD: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(?:^|/)(downloads?|다운로드)(?:/|$)").unwrap());
static RE_EDITED: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)[-_ ](edited|edit|수정)(?:[-_ .(]|$)").unwrap());

/// Rule-derived labels for one file.
pub fn rule_labels(rel_path: &str, name: &str, kind: &str, m: &Meta) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let has_camera = m.camera.is_some();
    if RE_SCREENSHOT.is_match(rel_path) && kind != "raw" {
        out.insert("screenshot".to_string());
    } else if kind == "photo" && !has_camera && name.to_ascii_lowercase().ends_with(".png") {
        // PNGs with no camera info and a screen-like aspect ratio are almost always screenshots.
        if let (Some(w), Some(h)) = (m.width, m.height) {
            let (a, b) = if w > h { (w, h) } else { (h, w) };
            let ratio = a as f64 / b.max(1) as f64;
            if (1.7..=2.4).contains(&ratio) && b >= 600 {
                out.insert("screenshot".to_string());
            }
        }
    }
    if RE_KAKAO.is_match(rel_path) {
        out.insert("src:kakaotalk".into());
    }
    if RE_WHATSAPP.is_match(rel_path) {
        out.insert("src:whatsapp".into());
    }
    if RE_TELEGRAM.is_match(rel_path) {
        out.insert("src:telegram".into());
    }
    if RE_LINE.is_match(rel_path) {
        out.insert("src:line".into());
    }
    if RE_INSTAGRAM.is_match(rel_path) {
        out.insert("src:instagram".into());
    }
    if RE_DOWNLOAD.is_match(rel_path) {
        out.insert("src:download".into());
    }
    if RE_EDITED.is_match(name) {
        out.insert("edited".into());
    }
    if let Some(c) = &m.camera {
        out.insert(format!("camera:{c}"));
    }
    if m.lat.is_some() && m.lon.is_some() {
        out.insert("gps".into());
    }
    if crate::meta::confidence(&m.date_source) == "low" {
        out.insert("date:uncertain".into());
    }
    out
}

pub fn replace_rule_labels(conn: &Connection, file_id: i64, labels: &BTreeSet<String>) -> Result<()> {
    conn.execute("DELETE FROM labels WHERE file_id = ? AND source = 'rule'", [file_id])?;
    let mut st = conn.prepare_cached("INSERT OR IGNORE INTO labels(file_id, label, source) VALUES (?, ?, 'rule')")?;
    for l in labels {
        st.execute(params![file_id, l])?;
    }
    Ok(())
}

pub fn normalize_user_label(label: &str) -> Option<String> {
    let l = label.trim();
    if l.is_empty() || l.len() > 100 {
        return None;
    }
    Some(l.to_string())
}

pub fn add_user_label(conn: &mut Connection, file_ids: &[i64], label: &str) -> Result<usize> {
    let Some(label) = normalize_user_label(label) else { return Ok(0) };
    let tx = conn.transaction()?;
    let mut n = 0;
    {
        let mut st = tx.prepare("INSERT INTO labels(file_id, label, source) VALUES (?, ?, 'user')
                                 ON CONFLICT(file_id, label) DO UPDATE SET source = 'user'")?;
        for id in file_ids {
            n += st.execute(params![id, label])?;
        }
    }
    tx.commit()?;
    Ok(n)
}

pub fn remove_label(conn: &mut Connection, file_ids: &[i64], label: &str) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut n = 0;
    {
        let mut st = tx.prepare("DELETE FROM labels WHERE file_id = ? AND label = ?")?;
        for id in file_ids {
            n += st.execute(params![id, label])?;
        }
    }
    tx.commit()?;
    Ok(n)
}

#[derive(Clone, Debug, Serialize)]
pub struct LabelRow {
    pub label: String,
    pub source: String,
}

pub fn labels_for(conn: &Connection, file_id: i64) -> Result<Vec<LabelRow>> {
    let mut st = conn.prepare("SELECT label, source FROM labels WHERE file_id = ? ORDER BY source DESC, label")?;
    let rows = st
        .query_map([file_id], |r| Ok(LabelRow { label: r.get(0)?, source: r.get(1)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Clone, Debug, Serialize)]
pub struct LabelCount {
    pub label: String,
    pub count: i64,
    pub user: bool,
}

pub fn label_counts(conn: &Connection) -> Result<Vec<LabelCount>> {
    let mut st = conn.prepare(
        "SELECT l.label, COUNT(*), MAX(l.source = 'user') FROM labels l JOIN files f ON f.id = l.file_id
         WHERE f.missing = 0 AND f.quarantined = 0 GROUP BY l.label ORDER BY COUNT(*) DESC",
    )?;
    let rows = st
        .query_map([], |r| Ok(LabelCount { label: r.get(0)?, count: r.get(1)?, user: r.get::<_, i64>(2)? != 0 }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Manually correct the capture date of files (the "correct label" for time).
pub fn set_taken_at(conn: &mut Connection, file_ids: &[i64], ts: i64) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut n = 0;
    {
        let mut st = tx.prepare("UPDATE files SET taken_at = ?, date_source = 'user' WHERE id = ?")?;
        let mut del = tx.prepare("DELETE FROM labels WHERE file_id = ? AND label = 'date:uncertain'")?;
        for id in file_ids {
            n += st.execute(params![ts, id])?;
            del.execute([id])?;
        }
    }
    tx.commit()?;
    Ok(n)
}

/// Shift dates by a delta (fixes a camera whose clock was wrong for a whole trip).
pub fn shift_taken_at(conn: &mut Connection, file_ids: &[i64], delta_secs: i64) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut n = 0;
    {
        let mut st = tx.prepare("UPDATE files SET taken_at = taken_at + ?, date_source = 'user' WHERE id = ? AND taken_at IS NOT NULL")?;
        for id in file_ids {
            n += st.execute(params![delta_secs, id])?;
        }
    }
    tx.commit()?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        let m = Meta { date_source: "exif".into(), camera: Some("Apple iPhone 13".into()), ..Default::default() };
        let l = rule_labels("Phone/DCIM/IMG_1.jpg", "IMG_1.jpg", "photo", &m);
        assert!(l.contains("camera:Apple iPhone 13"));
        assert!(!l.contains("screenshot"));

        let m = Meta { date_source: "filename".into(), width: Some(1080), height: Some(2340), ..Default::default() };
        let l = rule_labels("Pictures/Screenshots/Screenshot_20200101-000000.png", "Screenshot_20200101-000000.png", "photo", &m);
        assert!(l.contains("screenshot"));

        let l = rule_labels("Pictures/x/capture.png", "a.png", "photo", &m);
        assert!(l.contains("screenshot"), "phone-shaped png");

        let m = Meta { date_source: "mtime".into(), ..Default::default() };
        let l = rule_labels("KakaoTalk/KakaoTalk_20210304_101112345.jpg", "KakaoTalk_20210304_101112345.jpg", "photo", &m);
        assert!(l.contains("src:kakaotalk"));
        assert!(l.contains("date:uncertain"));
        let l = rule_labels("WhatsApp Images/IMG-20190512-WA0001.jpg", "IMG-20190512-WA0001.jpg", "photo", &m);
        assert!(l.contains("src:whatsapp"));
        let l = rule_labels("Users/me/Downloads/x.jpg", "x.jpg", "photo", &m);
        assert!(l.contains("src:download"));
    }
}

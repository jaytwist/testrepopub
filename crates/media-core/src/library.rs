//! The library layout: where every photo and video belongs in the one organised library, as
//! `year / year-month / event`, decided from what is already indexed (nothing is read again).
//!
//! * **Events** keep the name of the folder they came from (`2015-08-14 제주도` → `제주도`); the
//!   topmost meaningful folder that covers at most `event_span_days` is the event, and folders
//!   inside it stay as they are (`유럽여행/파리`). Collections spanning longer (`Family`, phone
//!   backups) are not events: their photos are sorted by date like loose ones.
//! * **Loose** photos go to `year/year-month`, or to the trip they belong to (`Honolulu 여행`);
//!   screenshots to `…/스크린샷`; photos received from messengers / downloads to
//!   `받은 사진/year/year-month`; undated ones to `_날짜확인필요/<folder>`.
//! * Videos of `big_video_bytes` or more go under `큰 동영상/`.
//! * **Pairs** travel together: a RAW goes into `RAW/` under its JPG, a Live Photo's video stays
//!   next to its photo, an edited version next to its original.
//! * **Copies**: of identical files one is kept — preferably one already on the library drive,
//!   which is moved; a file only on another drive is copied (that drive is left as it is). Other
//!   copies on the library drive are removed (quarantine, undoable); on other drives left alone.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::{self, FileRow};
use crate::dedupe::{self, DupRules};
use crate::meta;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LibraryOptions {
    pub big_video_bytes: i64,
    /// A folder whose photos span at most this many days is one event (kept whole).
    pub event_span_days: i64,
    /// Remove identical copies on the library drive (one is kept).
    pub remove_copies: bool,
    /// Resized / recompressed versions on other drives are not brought in when the original is
    /// kept (on the library drive they go beside their original, for review).
    pub remove_smaller_versions: bool,
    /// Edited versions of one photo: "latest" (the original and the newest edit stay; the other
    /// edits are offered for deletion in the year's review), "all" (all stay), "none" (only the
    /// original; every edit is offered for deletion). Nothing is deleted without the review.
    pub keep_edits: String,
}

impl Default for LibraryOptions {
    fn default() -> Self {
        Self { big_video_bytes: 2 * 1024 * 1024 * 1024, event_span_days: 62, remove_copies: true, remove_smaller_versions: true, keep_edits: "latest".into() }
    }
}

/// The user's correction for one source folder, remembered across plans.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FolderRule {
    pub volume_id: i64,
    /// Volume-relative folder, `/` separated.
    pub rel_dir: String,
    /// "event" (always an event, even if long; `name` renames it), "loose" (never an event),
    /// "skip" (leave this folder where it is).
    pub kind: String,
    pub name: Option<String>,
}

pub fn load_rules(conn: &Connection) -> Result<Vec<FolderRule>> {
    let v: Option<String> = conn.query_row("SELECT value FROM settings WHERE key = 'library_rules'", [], |r| r.get(0)).optional()?;
    Ok(v.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
}

pub fn save_rules(conn: &Connection, rules: &[FolderRule]) -> Result<()> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES ('library_rules', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [serde_json::to_string(rules)?],
    )?;
    Ok(())
}

fn load_setting<T: serde::de::DeserializeOwned + Default>(conn: &Connection, key: &str) -> Result<T> {
    let v: Option<String> = conn.query_row("SELECT value FROM settings WHERE key = ?", [key], |r| r.get(0)).optional()?;
    Ok(v.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
}

fn save_setting<T: Serialize>(conn: &Connection, key: &str, value: &T) -> Result<()> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, serde_json::to_string(value)?],
    )?;
    Ok(())
}

/// "Merge these library folders": what is planned into `from` itself (and its `RAW` folder)
/// goes into `into` instead. Other folders below `from` are folders of their own in the preview
/// and stay where they are.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Merge {
    pub from: String,
    pub into: String,
}

pub fn load_merges(conn: &Connection) -> Result<Vec<Merge>> {
    load_setting(conn, "library_merges")
}

pub fn save_merges(conn: &Connection, merges: &[Merge]) -> Result<()> {
    save_setting(conn, "library_merges", &merges)
}

/// Where a library folder ends up after the merges (followed through chains, A → B → C).
pub fn merged(rel_dir: &str, merges: &[Merge]) -> String {
    let (mut base, raw) = match rel_dir.strip_suffix("/RAW") {
        Some(b) => (b.to_string(), true),
        None => (rel_dir.to_string(), false),
    };
    for _ in 0..16 {
        let Some(m) = merges.iter().find(|m| m.from == base && m.into != m.from) else { break };
        base = m.into.clone();
    }
    if raw {
        base.push_str("/RAW");
    }
    base
}

/// Contents (fingerprints) the user deleted from the library for good: copies of them left on
/// other drives are not brought back.
pub fn load_excluded(conn: &Connection) -> Result<Vec<String>> {
    load_setting(conn, "library_excluded")
}

pub fn save_excluded(conn: &Connection, keys: &[String]) -> Result<()> {
    save_setting(conn, "library_excluded", &keys)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Act {
    Move,
    Copy,
    /// A copy on another drive, or a folder the user chose to skip: not touched.
    Leave,
    /// A copy on the library drive: removed (quarantined) once its kept twin is in place.
    Remove,
}

#[derive(Clone, Debug, Serialize)]
pub struct Placement {
    pub act: Act,
    /// Folder inside the library, `/` separated ("" for Leave / Remove).
    pub rel_dir: String,
    /// Why it goes there: event, loose, trip, received, screenshot, undated, raw, live, edited,
    /// copy, version, skip.
    pub tag: &'static str,
    /// The event (or trip) name, when there is one.
    pub event: Option<String>,
    /// The source folder (volume-relative) the event or file came from.
    pub from_dir: String,
    /// For Remove: the file kept instead.
    pub kept: Option<i64>,
}

// ---------------------------------------------------------------------------------------------
// Folder names
// ---------------------------------------------------------------------------------------------

const GENERIC: &[&str] = &[
    "dcim", "misc", "private", "avchd", "bdmv", "stream", "clip", "m4root", "camera", "camera roll", "camera uploads", "photos", "photo",
    "pictures", "picture", "images", "image", "img", "사진", "그림", "사진 모음", "내 사진", "download", "downloads", "다운로드",
    "kakaotalk", "kakao", "카카오톡", "카카오톡 받은 파일", "whatsapp", "whatsapp images", "whatsapp video", "telegram", "telegram images",
    "line", "instagram", "screenshots", "screenshot", "스크린샷", "화면 캡처", "screen recordings", "new folder", "새 폴더", "untitled folder",
    "untitled", "무제", "backup", "backups", "백업", "raw", "raws", "jpg", "jpeg", "heic", "video", "videos", "movies", "동영상", "영상",
    "export", "exports", "edited", "edit", "보정", "편집", "takeout", "google photos", "google 포토", "icloud photos", "icloud", "onedrive",
    "dropbox", "google drive", "desktop", "바탕 화면", "documents", "문서", "users", "user", "home", "media", "files", "data", "temp", "tmp",
    "받은 사진", "큰 동영상", "_날짜확인필요", "기타", "etc", "misc", "all", "sdcard", "internal storage", "phone", "iphone", "android",
    "폰", "폰사진", "폰 사진", "휴대폰", "휴대폰 사진", "핸드폰", "핸드폰 사진", "스마트폰", "카메라", "캠코더", "camcorder", "phone photos", "mobile",
    "사진 백업", "옛날사진", "옛날 사진", "old photos", "정리", "미정리", "정리 안 됨", "unsorted", "inbox", "import", "imports", "가져오기",
];

/// Folders a recovery or repair program made (PhotoRec `recup_dir.12`, `run_20261110_021654`,
/// `untrunc…`, `Raw2`): nothing below them was named by a person — their names are not events,
/// and a file there is not the one to keep when a copy of it sits elsewhere.
static RE_TOOL_DIR: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^(recup_dir\.\d+|run_\d+.*|untrunc.*|raw\d+|found\.\d+|lost\+found|\$recycle\.bin)$").unwrap());
/// File names a recovery program gives (`f1234567.jpg`, `file0012.jpg`, `recup_…`).
static RE_TOOL_NAME: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^(f\d{4,}|file\d{3,}|recup.*|recovered.*|_?\d{5,})(\.[a-z0-9]+)?$").unwrap());

/// The folders of a path up to (not including) the first one a recovery program made.
fn named_part(dir: &str) -> impl Iterator<Item = &str> {
    dir.split('/').filter(|s| !s.is_empty()).take_while(|s| !RE_TOOL_DIR.is_match(s))
}

/// Whether a path lies below a folder a recovery program made.
pub fn in_tool_dir(rel: &str) -> bool {
    let rel = rel.replace('\\', "/");
    let dir = rel.rfind('/').map(|i| &rel[..i]).unwrap_or("");
    dir.split('/').any(|s| RE_TOOL_DIR.is_match(s))
}

/// Whether a file name was given by a recovery program, not a camera or a person.
pub fn tool_named(name: &str) -> bool {
    RE_TOOL_NAME.is_match(name)
}

/// Leading date (and an optional range) in a folder name: `2015-08-14 제주도`, `20150814_제주`,
/// `2015.08 여행`, `2015-08-14~16 제주`, `2015`.
static RE_LEAD_DATE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^\s*((?:19|20)\d{2})(?:[-_. ]?(0[1-9]|1[0-2])(?:[-_. ]?(0[1-9]|[12]\d|3[01]))?)?(?:\s*[~\-]\s*\d{1,4}(?:[-_. ]\d{1,2}){0,2})?(?:[\s_\-.,:)]+|$)",
    )
    .unwrap()
});
/// `2024년 6월 (12일)` written the Korean way, turned into `2024-06(-12)`.
static RE_KO_DATE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*((?:19|20)\d{2})\s*년\s*(\d{1,2})\s*월(?:\s*(\d{1,2})\s*일)?").unwrap());

fn normalize_date(s: &str) -> String {
    match RE_KO_DATE.captures(s) {
        Some(c) => {
            let m: u32 = c[2].parse().unwrap_or(0);
            let head = match c.get(3).and_then(|d| d.as_str().parse::<u32>().ok()) {
                Some(d) => format!("{}-{m:02}-{d:02}", &c[1]),
                None => format!("{}-{m:02}", &c[1]),
            };
            format!("{head}{}", &s[c.get(0).map(|m| m.end()).unwrap_or(0)..])
        }
        None => s.to_string(),
    }
}

static RE_CAMERA_DIR: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\d{3}[A-Z0-9_]{5}$").unwrap());
static RE_PHOTOS_FROM: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^photos from \d{4}$").unwrap());

/// The event name a folder stands for, or None when the name says nothing (camera / app /
/// generic folders, dates alone, numbers).
pub fn event_name(folder: &str) -> Option<String> {
    let normalized = normalize_date(folder);
    let f = normalized.trim();
    if f.is_empty() || RE_CAMERA_DIR.is_match(f) || RE_PHOTOS_FROM.is_match(f) || GENERIC.contains(&f.to_lowercase().as_str()) {
        return None;
    }
    let rest = match RE_LEAD_DATE.find(f) {
        Some(m) => &f[m.end()..],
        None => f,
    };
    let rest = rest.trim_matches(|c: char| c.is_whitespace() || "-_.,~()[]".contains(c));
    if rest.is_empty() || rest.chars().all(|c| c.is_ascii_digit() || " -_.".contains(c)) {
        return None;
    }
    let lower = rest.to_lowercase();
    if GENERIC.contains(&lower.as_str()) {
        return None;
    }
    // "옛날 백업", "Photos backup 2", "사진 복사본": a backup of something, not an event.
    if ["백업", "backup", "사본", "복사본", "copy", "copies", "recovery", "recovered", "복구"].iter().any(|w| lower.contains(w)) || RE_TOOL_DIR.is_match(rest) {
        return None;
    }
    Some(safe_name(rest))
}

/// (year, month) written at the start of a folder name, if any.
pub fn folder_month(folder: &str) -> Option<(i32, u32)> {
    let folder = normalize_date(folder);
    let c = RE_LEAD_DATE.captures(&folder)?;
    let y: i32 = c.get(1)?.as_str().parse().ok()?;
    let m: u32 = c.get(2)?.as_str().parse().ok()?;
    Some((y, m))
}

/// How many folders of a file's path say something (an event rather than "backup").
fn meaning(r: &FileRow) -> usize {
    let rel = r.rel_path.replace('\\', "/");
    let dir = rel.rfind('/').map(|i| &rel[..i]).unwrap_or("");
    named_part(dir).filter(|s| event_name(s).is_some()).count()
}

/// Whether a file is already inside the library folder.
pub fn is_filed(r: &FileRow, lib_vol: i64, lib_root: &str) -> bool {
    r.volume_id == lib_vol && (lib_root.is_empty() || r.rel_path.replace('\\', "/").starts_with(&format!("{lib_root}/")))
}

/// Among identical files (already in the duplicate ranking's order), the one kept goes first:
/// on the library drive, already filed in the library, in the most meaningful folders. One rule
/// for the organizer and for the "original" shown everywhere.
pub fn keeper_first(rows: &mut [FileRow], lib_vol: i64, lib_root: &str) {
    // Then: not in a recovery program's output, a name a camera or a person gave, the most
    // meaningful folders.
    rows.sort_by_key(|r| {
        (
            (r.volume_id != lib_vol) as u8,
            !is_filed(r, lib_vol, lib_root) as u8,
            in_tool_dir(&r.rel_path) as u8,
            tool_named(&r.name) as u8,
            std::cmp::Reverse(meaning(r)),
        )
    });
}

/// The library drive and folder (relative to the drive) the user chose, if any.
pub fn location(conn: &Connection) -> Option<(i64, String)> {
    let dir = crate::importer::ImportSettings::load(conn).ok()?.target_dir;
    if dir.trim().is_empty() {
        return None;
    }
    let target = crate::volumes::canonical_lenient(std::path::Path::new(dir.trim())).ok()?;
    let vol = crate::volumes::identify_lenient(&target).ok()?;
    let id: i64 = conn.query_row("SELECT id FROM volumes WHERE uid = ?", [&vol.uid], |r| r.get(0)).ok()?;
    Some((id, crate::volumes::rel_of(&vol.root, &target).unwrap_or_default().replace('\\', "/").trim_matches('/').to_string()))
}

/// Why `first` is the original among identical files `rest` (in words, for the user): every
/// reason that set it apart from one of the others.
pub fn why_original(first: &FileRow, rest: &[FileRow], marked: bool, lib: Option<(i64, &str)>, rules: &DupRules) -> String {
    if marked {
        return "직접 원본으로 지정함".into();
    }
    let mut parts: Vec<&str> = Vec::new();
    if let Some((vol, root)) = lib {
        if is_filed(first, vol, root) {
            return "라이브러리에 정리되어 있음".into();
        }
        if first.volume_id == vol && rest.iter().any(|r| r.volume_id != vol) {
            parts.push("라이브러리 드라이브에 있음");
        }
    }
    if rules.is_kept(first) {
        parts.push("「항상 남길 폴더」에 있음");
    }
    let beside: Vec<&FileRow> = rest.iter().filter(|r| r.volume_id == first.volume_id).collect();
    if !beside.is_empty() && beside.iter().all(|r| meaning(first) > meaning(r)) {
        parts.push("이름 있는 폴더(이벤트)에 있음");
    }
    if parts.is_empty() {
        return "같은 파일 중 자동 선택 (날짜가 확실하고 먼저 만들어진 것)".into();
    }
    parts.join(" · ")
}

/// A folder name Windows accepts: no reserved characters or names, no trailing dot / space,
/// at most 60 characters.
pub fn safe_name(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .take(60)
        .collect::<String>()
        .trim_end_matches(['.', ' '])
        .trim()
        .to_string();
    let upper = out.to_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&upper.as_str())
        || (upper.len() == 4 && (upper.starts_with("COM") || upper.starts_with("LPT")) && upper.as_bytes()[3].is_ascii_digit());
    if reserved {
        out.push('_');
    }
    if out.is_empty() {
        out = "_".into();
    }
    out
}

fn month_dir(ts: i64) -> Option<String> {
    let d = meta::from_ts(ts)?;
    use chrono::Datelike;
    Some(format!("{:04}/{:04}-{:02}", d.year(), d.year(), d.month()))
}

// ---------------------------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------------------------

struct F {
    id: i64,
    vol: i64,
    rel: String,
    kind: String,
    size: i64,
    taken: Option<i64>,
    trusted: bool,
    content: Option<(String, i64)>,
    /// No fingerprint yet while another file has the same size: it may be a copy — left alone
    /// until the fingerprint is read (in the background).
    unverified: bool,
}

impl F {
    fn dir(&self) -> &str {
        self.rel.rfind('/').map(|i| &self.rel[..i]).unwrap_or("")
    }
    /// (drive, folder, lower-cased name without extension). A RAW already filed in a `RAW`
    /// subfolder counts as beside the JPG in the folder above, so organizing twice changes nothing.
    fn stem_key(&self) -> (i64, String, String) {
        let name = self.rel.rsplit('/').next().unwrap_or("");
        let stem = name.rfind('.').map(|i| &name[..i]).unwrap_or(name);
        let mut dir = self.dir();
        if self.kind == "raw" && dir.rsplit('/').next().is_some_and(|d| d.eq_ignore_ascii_case("raw")) {
            dir = dir.rfind('/').map(|i| &dir[..i]).unwrap_or("");
        }
        (self.vol, dir.to_string(), stem.to_lowercase())
    }
}

/// Where each of `ids` belongs in the library on volume `lib_vol`.
/// `lib_root` is the library folder relative to its drive (`사진`): a copy already filed there is
/// the one kept.
/// `all` is every indexed file (`db::all_files`), loaded once by the caller.
pub fn layout(
    conn: &Connection,
    lib_vol: i64,
    lib_root: &str,
    ids: &[i64],
    opts: &LibraryOptions,
    rules: &[FolderRule],
    all: &HashMap<i64, FileRow>,
) -> Result<HashMap<i64, Placement>> {
    let lib_root = lib_root.replace('\\', "/").trim_matches('/').to_string();
    let wanted: HashSet<i64> = ids.iter().copied().collect();
    // id -> (content, folder) of filed library files outside the plan (see below).
    let anchors: HashMap<i64, (Option<(String, i64)>, String)>;
    let files: Vec<F> = {
        let mut st = conn.prepare(
            "SELECT id, volume_id, rel_path, kind, size, taken_at, date_source, COALESCE(prehash, hash)
             FROM files WHERE missing = 0 AND quarantined = 0",
        )?;
        let rows = st
            .query_map([], |r| {
                let src: String = r.get(6)?;
                let key: Option<String> = r.get(7)?;
                let size: i64 = r.get(4)?;
                Ok(F {
                    id: r.get(0)?,
                    vol: r.get(1)?,
                    rel: r.get::<_, String>(2)?.replace('\\', "/"),
                    kind: r.get(3)?,
                    size,
                    taken: r.get(5)?,
                    trusted: meta::confidence(&src) != "low",
                    // '' = the fingerprint could not be read: never the same as anything.
                    unverified: key.is_none(),
                    content: key.filter(|k| !k.is_empty()).map(|k| (k, size)),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut sizes: HashMap<i64, u32> = HashMap::new();
        for f in &rows {
            *sizes.entry(f.size).or_default() += 1;
        }
        // Files already filed in the library that are not part of this plan (it covers only the
        // work scope): they stay where they are, and a photo of the plan the same as one of them
        // is a copy of it — not brought into the library a second time.
        anchors = rows
            .iter()
            .filter(|f| !wanted.contains(&f.id) && f.vol == lib_vol && (lib_root.is_empty() || f.rel.starts_with(&format!("{lib_root}/"))))
            .map(|f| (f.id, (f.content.clone(), f.dir().to_string())))
            .collect();
        rows.into_iter()
            .filter(|f| wanted.contains(&f.id))
            .map(|mut f| {
                f.unverified = f.unverified && sizes.get(&f.size).copied().unwrap_or(0) > 1;
                f
            })
            .collect()
    };
    let by_id: HashMap<i64, usize> = files.iter().enumerate().map(|(i, f)| (f.id, i)).collect();
    // Photos the organizer already moved into the library, at the place each came from. Whether
    // a folder is one event is a fact about the folder as the user made it — not about what
    // happens to be left in it after some years were pushed: `옛날`, holding photos of many
    // years, must not turn into "an event" once only one spring remains. (A move that was
    // undone is no longer here: its photo is back in the folder and counts as itself.)
    let moved_out: Vec<F> = {
        let mut st = conn.prepare(
            "SELECT j.src_volume, j.src_rel, f.taken_at, f.date_source FROM journal j JOIN files f ON f.id = j.file_id
             WHERE j.op = 'move' AND j.undone = 0 AND f.missing = 0 AND f.quarantined = 0",
        )?;
        let rows = st
            .query_map([], |r| {
                let src: String = r.get(3)?;
                Ok(F {
                    id: 0,
                    vol: r.get(0)?,
                    rel: r.get::<_, String>(1)?.replace('\\', "/"),
                    kind: String::new(),
                    size: 0,
                    taken: r.get(2)?,
                    trusted: meta::confidence(&src) != "low",
                    content: None,
                    unverified: false,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // Moves inside the library (merged folders) say nothing about a source folder.
        rows.into_iter().filter(|g| !(g.vol == lib_vol && (lib_root.is_empty() || g.rel.starts_with(&format!("{lib_root}/"))))).collect()
    };
    // Trips, loaded once (not one query per file).
    let trips: HashMap<i64, (String, i64)> = {
        let mut st = conn.prepare("SELECT tf.file_id, t.title, t.start_ts FROM trip_files tf JOIN trips t ON t.id = tf.trip_id")?;
        let m = st.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?.collect::<rusqlite::Result<HashMap<i64, (String, i64)>>>()?;
        m
    };
    let mut labels: HashMap<i64, Vec<String>> = HashMap::new();
    {
        let mut st = conn.prepare("SELECT file_id, label FROM labels")?;
        for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (id, l) = r?;
            if wanted.contains(&id) {
                labels.entry(id).or_default().push(l);
            }
        }
    }
    let has = |id: i64, p: &dyn Fn(&str) -> bool| labels.get(&id).is_some_and(|ls| ls.iter().any(|l| p(l)));

    // 1. Copies: keep one per content (preferably on the library drive), by the duplicate ranking.
    let mut act: HashMap<i64, (Act, Option<i64>, &'static str)> = HashMap::new();
    // Deleted from the library for good: copies elsewhere stay where they are.
    let excluded: HashSet<String> = load_excluded(conn)?.into_iter().collect();
    for f in files.iter().filter(|f| f.content.as_ref().is_some_and(|c| excluded.contains(&c.0))) {
        act.insert(f.id, (Act::Leave, None, "excluded"));
    }
    let mut groups: HashMap<(String, i64), Vec<i64>> = HashMap::new();
    for f in &files {
        if let Some(c) = &f.content {
            if !excluded.contains(&c.0) {
                groups.entry(c.clone()).or_default().push(f.id);
            }
        }
    }
    // Filed photos outside the plan join only the groups of photos in it.
    for (id, (content, _)) in &anchors {
        if let Some(g) = content.as_ref().and_then(|c| groups.get_mut(c)) {
            g.push(*id);
        }
    }
    let dup_rules = DupRules::load(conn)?;
    for members in groups.values().filter(|m| m.len() > 1) {
        let mut rows: Vec<FileRow> = members.iter().filter_map(|id| all.get(id).cloned()).collect();
        dedupe::rank_members(&mut rows, &labels, &dup_rules);
        // Identical bytes: keep the one on the library drive — one already in the library folder
        // first — then the one filed under the most meaningful folders (an event rather than
        // "backup"), then the duplicate ranking.
        keeper_first(&mut rows, lib_vol, &lib_root);
        let keep = rows[0].id;
        for (i, r) in rows.iter().enumerate() {
            if anchors.contains_key(&r.id) {
                continue; // not part of this plan: stays as it is
            }
            let a = if i == 0 {
                (if r.volume_id == lib_vol { Act::Move } else { Act::Copy }, None, "")
            } else if r.volume_id == lib_vol && opts.remove_copies {
                (Act::Remove, Some(keep), "copy")
            } else {
                (Act::Leave, Some(keep), "copy")
            };
            act.insert(r.id, a);
        }
    }
    for f in files.iter().filter(|f| f.unverified) {
        act.insert(f.id, (Act::Leave, None, "unverified"));
    }
    for f in &files {
        act.entry(f.id).or_insert((if f.vol == lib_vol { Act::Move } else { Act::Copy }, None, ""));
    }

    // 2. Versions of a similar photo: resized / recompressed ones go like copies; edited ones
    //    follow their original.
    let mut follow: HashMap<i64, (i64, &'static str, &'static str)> = HashMap::new(); // id -> (leader, subdir, tag)
    {
        let mut st = conn.prepare(
            "SELECT m.group_id, m.file_id, m.variant FROM dup_members m JOIN dup_groups g ON g.id = m.group_id
             WHERE g.kind = 'similar_image' AND g.resolved = 0 ORDER BY m.group_id, m.rank",
        )?;
        // A member whose content is kept elsewhere (a copy already in the library) stands for that
        // copy, so an edited version still finds its original after the original was copied in.
        let stand_in: HashMap<i64, i64> =
            act.iter().filter_map(|(id, (a, kept, _))| matches!(a, Act::Leave | Act::Remove).then_some(()).and(kept.map(|k| (*id, k)))).collect();
        let mut by_group: HashMap<i64, Vec<(i64, Option<String>)>> = HashMap::new();
        for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?)))? {
            let (g, id, v) = r?;
            if wanted.contains(&id) || anchors.contains_key(&id) {
                let id = stand_in.get(&id).copied().unwrap_or(id);
                let members = by_group.entry(g).or_default();
                if !members.iter().any(|(m, _)| *m == id) {
                    members.push((id, v));
                }
            }
        }
        // A version travels with its original only when taken around the same time (or with no
        // trusted date): look-alike shots from other days are filed by their own date.
        let near = |a: i64, b: i64| -> bool {
            match (by_id.get(&a).map(|&i| &files[i]), by_id.get(&b).map(|&i| &files[i])) {
                (Some(x), Some(y)) => match (x.taken, y.taken) {
                    (Some(p), Some(q)) if x.trusted && y.trusted => (p - q).abs() <= 86_400,
                    _ => true,
                },
                _ => true,
            }
        };
        for members in by_group.values() {
            let Some(&(lead, _)) = members.iter().find(|(_, v)| v.is_none()) else { continue };
            if !anchors.contains_key(&lead) && !matches!(act.get(&lead).map(|a| a.0), Some(Act::Move | Act::Copy)) {
                continue;
            }
            for (id, v) in members {
                match v.as_deref() {
                    // Resized / recompressed versions: not deleted on their own (they only look
                    // alike) — on the library drive they go beside their original and are offered
                    // in the year's 「비슷한 사진」 review; on other drives they are not brought in.
                    Some("smaller" | "recompressed") => match act.get(id).map(|a| a.0) {
                        Some(Act::Copy) if opts.remove_smaller_versions => {
                            act.insert(*id, (Act::Leave, Some(lead), "version"));
                        }
                        Some(Act::Move | Act::Copy) if near(*id, lead) => {
                            follow.insert(*id, (lead, "", "version"));
                        }
                        _ => {}
                    },
                    Some("edited") if near(*id, lead) => {
                        follow.insert(*id, (lead, "", "edited"));
                    }
                    _ => {}
                }
            }
        }
    }

    // 3. Same name in the same folder: RAW under its JPG, a Live Photo's video beside its photo.
    {
        let mut by_stem: HashMap<(i64, String, String), Vec<usize>> = HashMap::new();
        for (i, f) in files.iter().enumerate() {
            by_stem.entry(f.stem_key()).or_default().push(i);
        }
        for list in by_stem.values().filter(|l| l.len() > 1) {
            let Some(&lead) = list.iter().find(|&&i| files[i].kind == "photo") else { continue };
            for &i in list.iter().filter(|&&i| i != lead) {
                match files[i].kind.as_str() {
                    "raw" => {
                        follow.insert(files[i].id, (files[lead].id, "RAW", "raw"));
                    }
                    "video" => {
                        follow.insert(files[i].id, (files[lead].id, "", "live"));
                    }
                    _ => {}
                }
            }
        }
    }

    // 4. Events: per folder, the dates of everything below it.
    let mut roots: HashMap<i64, Vec<String>> = HashMap::new();
    for s in db::list_sources(conn)? {
        roots.entry(s.volume_id).or_default().push(s.rel_root.replace('\\', "/").trim_matches('/').to_string());
    }
    let root_of = |f: &F| -> String {
        roots
            .get(&f.vol)
            .and_then(|rs| rs.iter().filter(|r| r.is_empty() || f.rel.starts_with(&format!("{r}/"))).max_by_key(|r| r.len()).cloned())
            .unwrap_or_default()
    };
    let rule_of = |vol: i64, dir: &str| rules.iter().find(|r| r.volume_id == vol && r.rel_dir.trim_matches('/') == dir);
    // (vol, folder) -> (min, max) trusted date of everything inside
    let mut span: HashMap<(i64, String), (i64, i64)> = HashMap::new();
    let segs_of = |f: &F, root: &str| -> Vec<String> {
        let dir = f.dir();
        let below = if root.is_empty() { dir } else { dir.strip_prefix(root).unwrap_or(dir).trim_start_matches('/') };
        // A recovery program's folders (and what is below them) name nothing: such files are
        // filed by their date.
        named_part(below).map(String::from).collect()
    };
    let path_at = |root: &str, segs: &[String], i: usize| -> String {
        let p = segs[..=i].join("/");
        if root.is_empty() {
            p
        } else {
            format!("{root}/{p}")
        }
    };
    // Undated files (scans) count with the date written on their folder, so a folder holding
    // "2009-07 제주도" and "2023-12 유럽" is not taken for one event.
    let mid_month = |(y, m): (i32, u32)| meta::to_ts(chrono::NaiveDate::from_ymd_opt(y, m, 15).and_then(|d| d.and_hms_opt(0, 0, 0)).unwrap_or_default());
    for f in files.iter().chain(&moved_out) {
        let root = root_of(f);
        let segs = segs_of(f, &root);
        let t = match (f.taken, f.trusted) {
            (Some(t), true) => t,
            _ => match segs.iter().rev().find_map(|s| folder_month(s)) {
                Some(ym) => mid_month(ym),
                None => continue,
            },
        };
        for i in 0..segs.len() {
            let e = span.entry((f.vol, path_at(&root, &segs, i))).or_insert((t, t));
            e.0 = e.0.min(t);
            e.1 = e.1.max(t);
        }
    }
    let max_span = opts.event_span_days * 86_400;
    // Per folder: media files directly in it, and its meaningful subfolders. A folder holding no
    // photos itself and only one meaningful subfolder is a wrapper ("옛날/유럽 여행"), not the event.
    let mut direct: HashMap<(i64, String), usize> = HashMap::new();
    let mut kids: HashMap<(i64, String), HashSet<String>> = HashMap::new();
    for f in files.iter().chain(&moved_out) {
        let root = root_of(f);
        let segs = segs_of(f, &root);
        if let Some(last) = segs.len().checked_sub(1) {
            *direct.entry((f.vol, path_at(&root, &segs, last))).or_default() += 1;
        }
        for i in 1..segs.len() {
            if event_name(&segs[i]).is_some() {
                kids.entry((f.vol, path_at(&root, &segs, i - 1))).or_default().insert(segs[i].clone());
            }
        }
    }
    let wrapper = |vol: i64, path: &str| -> bool {
        let key = (vol, path.to_string());
        direct.get(&key).copied().unwrap_or(0) == 0 && kids.get(&key).map(|k| k.len()).unwrap_or(0) == 1
    };

    let mut out: HashMap<i64, Placement> = HashMap::new();
    let mut leaders: Vec<usize> = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let (a, kept, tag) = act[&f.id];
        if matches!(a, Act::Leave | Act::Remove) {
            out.insert(f.id, Placement { act: a, rel_dir: String::new(), tag, event: None, from_dir: f.dir().to_string(), kept });
        } else if !follow.contains_key(&f.id) {
            leaders.push(i);
        }
    }
    for i in leaders {
        let f = &files[i];
        let (a, _, _) = act[&f.id];
        let root = root_of(f);
        let segs = segs_of(f, &root);
        // Topmost meaningful folder that is one event (or that the user named one).
        let mut chosen: Option<(usize, String, Option<(i64, i64)>)> = None;
        let mut skip = false;
        for k in 0..segs.len() {
            let path = path_at(&root, &segs, k);
            let rule = rule_of(f.vol, &path);
            match rule.map(|r| r.kind.as_str()) {
                Some("skip") => {
                    skip = true;
                    break;
                }
                Some("loose") => continue,
                Some("event") => {
                    let name = rule.and_then(|r| r.name.clone()).map(|n| safe_name(&n)).or_else(|| event_name(&segs[k])).unwrap_or_else(|| safe_name(&segs[k]));
                    chosen = Some((k, name, span.get(&(f.vol, path)).copied()));
                    break;
                }
                _ => {}
            }
            let Some(name) = event_name(&segs[k]) else { continue };
            if wrapper(f.vol, &path) {
                continue;
            }
            let sp = span.get(&(f.vol, path)).copied();
            if sp.is_none_or(|(lo, hi)| hi - lo <= max_span) {
                chosen = Some((k, name, sp));
                break;
            }
        }
        if skip {
            out.insert(f.id, Placement { act: Act::Leave, rel_dir: String::new(), tag: "skip", event: None, from_dir: f.dir().to_string(), kept: None });
            continue;
        }
        let big = f.kind == "video" && f.size >= opts.big_video_bytes;
        let prefix = if big { "큰 동영상/" } else { "" };
        let (rel_dir, tag, event, from_dir) = if let Some((k, name, sp)) = chosen {
            // Month: the date written on the folder when it agrees with the photos, else the first photo.
            let first = sp.map(|s| s.0);
            // The folder's own date, else a date-only folder above it (`2009/2009-07/제주도` already in the library).
            let written = (0..=k).rev().find_map(|j| folder_month(&segs[j]));
            // Already filed in the library under a month folder: it stays in that month (the
            // event's photos may have been planned from several places; what is filed is not
            // moved again because the folder now holds other photos).
            let month_at = (0..k).rev().find(|&j| folder_month(&segs[j]).is_some());
            let filed = f.vol == lib_vol && (lib_root.is_empty() || f.rel.starts_with(&format!("{lib_root}/"))) && month_at.is_some();
            let month = match (written, first) {
                (Some((y, m)), _) if filed => Some(format!("{y:04}/{y:04}-{m:02}")),
                (Some((y, m)), Some(t)) => {
                    let named = mid_month((y, m));
                    if (named - t).abs() <= 31 * 86_400 {
                        Some(format!("{y:04}/{y:04}-{m:02}"))
                    } else {
                        month_dir(t)
                    }
                }
                (_, Some(t)) => month_dir(t),
                (Some((y, m)), None) => Some(format!("{y:04}/{y:04}-{m:02}")),
                (None, None) => None,
            };
            let sub: Vec<String> = segs[k + 1..].iter().filter_map(|s| event_name(s)).collect();
            let mut path = match month {
                Some(m) => format!("{prefix}{m}"),
                None => format!("{prefix}_날짜확인필요"),
            };
            // What is filed stays in its folder, not only in its month: the folders between
            // the month and the event (`2015-03/옛날/제주도`) are kept, though a folder holding
            // nothing but one event would be skipped when read as a source.
            if let (true, Some(j)) = (filed, month_at) {
                for s in &segs[j + 1..k] {
                    path.push('/');
                    path.push_str(s);
                }
            }
            path.push('/');
            path.push_str(&name);
            for s in &sub {
                path.push('/');
                path.push_str(s);
            }
            (path, "event", Some(name), path_at(&root, &segs, k))
        } else {
            let month = if f.trusted { f.taken.and_then(month_dir) } else { None };
            let received = has(f.id, &|l| matches!(l, "src:kakaotalk" | "src:whatsapp" | "src:telegram" | "src:line" | "src:instagram" | "src:download"));
            let trip: Option<(String, i64)> = trips.get(&f.id).cloned();
            match month {
                None => {
                    // Named like an event folder ("2024 People" → "People"), so planning again keeps it.
                    let parent = segs.last().map(|s| event_name(s).unwrap_or_else(|| safe_name(s))).unwrap_or_else(|| "기타".into());
                    (format!("{prefix}_날짜확인필요/{parent}"), "undated", None, f.dir().to_string())
                }
                Some(m) if received => (format!("받은 사진/{m}"), "received", None, f.dir().to_string()),
                Some(m) if has(f.id, &|l| l == "screenshot") => (format!("{m}/스크린샷"), "screenshot", None, f.dir().to_string()),
                Some(m) => match trip {
                    Some((title, start)) => {
                        let tm = month_dir(start).unwrap_or(m);
                        // "2024년 6월 Honolulu 여행" → "Honolulu 여행": the month folder says when.
                        let t = event_name(&title).unwrap_or_else(|| safe_name(&title));
                        (format!("{prefix}{tm}/{t}"), "trip", Some(t), f.dir().to_string())
                    }
                    None => (format!("{prefix}{m}"), "loose", None, f.dir().to_string()),
                },
            }
        };
        out.insert(f.id, Placement { act: a, rel_dir, tag, event, from_dir, kept: None });
    }

    // 5. Followers go where their leader goes (when the leader is placed and they are not copies).
    for (id, (lead, subdir, tag)) in &follow {
        if out.contains_key(id) {
            continue; // a copy (Leave / Remove) already decided
        }
        let Some(&i) = by_id.get(id) else { continue };
        let (a, _, _) = act[id];
        let lead_dir = out.get(lead).filter(|p| matches!(p.act, Act::Move | Act::Copy)).map(|p| (p.rel_dir.clone(), p.event.clone(), p.from_dir.clone())).or_else(|| {
            // The original is already filed in the library: beside it.
            anchors.get(lead).map(|(_, dir)| {
                let in_lib = if lib_root.is_empty() { dir.as_str() } else { dir.strip_prefix(&format!("{lib_root}/")).unwrap_or(dir) };
                (in_lib.to_string(), None, dir.clone())
            })
        });
        match lead_dir {
            Some((d, event, from)) => {
                let rel_dir = if subdir.is_empty() { d } else { format!("{d}/{subdir}") };
                out.insert(*id, Placement { act: a, rel_dir, tag, event, from_dir: from, kept: None });
            }
            None => {
                // Leader not placed (it is a copy itself): place by date like a loose file.
                let f = &files[i];
                let rel_dir = f.taken.filter(|_| f.trusted).and_then(month_dir).unwrap_or_else(|| "_날짜확인필요/기타".into());
                out.insert(*id, Placement { act: a, rel_dir, tag: "loose", event: None, from_dir: f.dir().to_string(), kept: None });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names() {
        assert_eq!(event_name("2015-08-14 제주도").as_deref(), Some("제주도"));
        assert_eq!(event_name("20150814_제주 여행").as_deref(), Some("제주 여행"));
        assert_eq!(event_name("2015.08 여름휴가").as_deref(), Some("여름휴가"));
        assert_eq!(event_name("2015-08-14~16 제주").as_deref(), Some("제주"));
        assert_eq!(event_name("하와이 여행").as_deref(), Some("하와이 여행"));
        for generic in ["DCIM", "100CANON", "101APPLE", "Camera Roll", "카카오톡 받은 파일", "Photos from 2019", "2019", "2019-05", "20190512", "새 폴더", "RAW", "0001"] {
            assert_eq!(event_name(generic), None, "{generic}");
        }
        assert_eq!(folder_month("2015-08-14 제주도"), Some((2015, 8)));
        assert_eq!(event_name("2024년 6월 Honolulu 여행").as_deref(), Some("Honolulu 여행"));
        assert_eq!(event_name("2019년 5월 12일 생일").as_deref(), Some("생일"));
        assert_eq!(folder_month("2024년 6월 Honolulu 여행"), Some((2024, 6)));
        assert_eq!(folder_month("제주도"), None);
        assert_eq!(event_name("옛날 백업"), None);
        assert_eq!(event_name("Photos Backup 2"), None);
        assert_eq!(event_name("사진 복사본"), None);
        assert_eq!(event_name("폰"), None);
        let m = vec![Merge { from: "2019/2019-05".into(), into: "2019/2019-12".into() }];
        assert_eq!(merged("2019/2019-05", &m), "2019/2019-12");
        assert_eq!(merged("2019/2019-05/RAW", &m), "2019/2019-12/RAW");
        assert_eq!(merged("2019/2019-05/제주도", &m), "2019/2019-05/제주도", "another folder of its own stays");
        assert_eq!(safe_name("a:b?c."), "a_b_c");
        assert_eq!(safe_name("CON"), "CON_");
        assert_eq!(safe_name(&"가".repeat(80)).chars().count(), 60);
    }
}

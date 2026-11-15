//! Organizer: gather media from every drive into one library folder using a path template.
//!
//! Always two steps: `plan()` (dry run — nothing touches the disk) and `execute()` (journaled,
//! undoable via `journal::undo_batch`). Conflict handling follows phockup: an identical file
//! already at the destination is skipped (left for the duplicate tool); a different file with
//! the same name gets a numeric suffix.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use chrono::Datelike;
use serde::{Deserialize, Serialize};

use crate::db::{self, FileFilter, FileRow};
use crate::{journal, meta, volumes, Db, Progress, ProgressFn, StopFlag};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Move,
    Copy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rename {
    /// Keep the original file name.
    Keep,
    /// `2019-05-12_143012_IMG_1234.jpg` — sorts chronologically in any file manager.
    DatePrefix,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct OrganizeOptions {
    pub target_dir: String,
    /// Tokens: {year} {month} {day} {date} {kind} {camera} {source} {label} {trip} {person}
    pub template: String,
    pub rename: Rename,
    pub mode: Mode,
    pub filter: FileFilter,
    /// Files whose date is only a guess (folder name / file time) go to this folder instead.
    pub uncertain_folder: Option<String>,
    pub remove_empty_dirs: bool,
    /// "template" (the `template` above) or "library" (year / year-month / event, from existing
    /// folder names — see `library`).
    pub layout: String,
    pub library: crate::library::LibraryOptions,
}

impl Default for OrganizeOptions {
    fn default() -> Self {
        Self {
            target_dir: String::new(),
            template: "{year}/{year}-{month}".into(),
            rename: Rename::Keep,
            mode: Mode::Move,
            filter: FileFilter::default(),
            uncertain_folder: Some("_날짜확인필요".into()),
            remove_empty_dirs: true,
            layout: "template".into(),
            library: Default::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Move,
    Copy,
    /// Already where the template says it should be.
    InPlace,
    /// Identical file already exists at the destination.
    SkipDuplicate,
    /// Drive not connected.
    SkipOffline,
    /// Library: a copy on another drive (or a skipped folder) — left as it is.
    Leave,
    /// Library: a copy on the library drive — quarantined once its kept twin is in place.
    Remove,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanItem {
    pub file_id: i64,
    pub src: String,
    pub dst: String,
    pub action: Action,
    pub renamed: bool,
    pub cross_volume: bool,
    pub size: i64,
    /// Library: folder inside the library (for the folder tree), why it goes there, its event,
    /// the folder it comes from, and for Remove the file kept instead.
    pub group: String,
    pub tag: String,
    pub event: Option<String>,
    pub from: String,
    pub kept: Option<i64>,
    /// The file's drive (with `from`, identifies the source folder for folder rules).
    pub vol: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PlanSummary {
    pub total: u64,
    pub moves: u64,
    pub copies: u64,
    pub in_place: u64,
    pub skip_duplicate: u64,
    pub skip_offline: u64,
    pub renamed: u64,
    pub cross_volume: u64,
    pub bytes: i64,
    pub uncertain_dates: u64,
    pub folders: u64,
    /// Library: copies on other drives left alone, copies on the library drive removed.
    pub leave: u64,
    pub remove: u64,
    pub remove_bytes: i64,
    /// Bytes copied from other drives (needs that much free space on the library drive).
    pub copy_bytes: i64,
    pub big_videos: u64,
    pub received: u64,
    /// RAW / Live Photo videos / edited versions kept with their photo.
    pub paired: u64,
    pub events: u64,
    /// Other files (documents, project files…) left behind in folders photos move out of.
    pub leftover_files: u64,
    /// Library: files whose fingerprint is not read yet while another file has the same size —
    /// they may be copies, so they are left alone until it is read (in the background).
    pub unverified: u64,
    /// How long each step of planning took (name, ms).
    pub timing: Vec<(String, u64)>,
    /// Library: photos the user deleted for good whose copies on other drives are not brought back.
    pub excluded: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub id: String,
    pub options: OrganizeOptions,
    pub items: Vec<PlanItem>,
    pub summary: PlanSummary,
    /// Library: the year of each file (worked out once; the year views use it on every click).
    #[serde(skip)]
    pub years: HashMap<i64, String>,
}

// ---------------------------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------------------------

const KIND_KO: &[(&str, &str)] = &[("photo", "사진"), ("video", "동영상"), ("raw", "RAW"), ("screenshot", "스크린샷")];

/// Windows-safe path component.
pub fn sanitize(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() { '_' } else { c })
        .collect();
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    let upper = out.to_ascii_uppercase();
    let stem = upper.split('.').next().unwrap_or("");
    let reserved = ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3"];
    if reserved.contains(&stem) {
        out.insert(0, '_');
    }
    if out.is_empty() {
        out.push('_');
    }
    out.chars().take(120).collect()
}

pub struct TemplateCtx<'a> {
    pub file: &'a FileRow,
    pub labels: &'a [String],
    /// Title of the automatic trip this file belongs to (`{trip}`; empty outside trips).
    pub trip: Option<&'a str>,
    /// Named people in the photo as one folder name (`{person}`; empty when nobody is named).
    pub person: Option<&'a str>,
}

fn primary_kind(ctx: &TemplateCtx) -> &'static str {
    if ctx.labels.iter().any(|l| l == "screenshot") {
        "screenshot"
    } else {
        match ctx.file.kind.as_str() {
            "video" => "video",
            "raw" => "raw",
            _ => "photo",
        }
    }
}

pub fn render_template(tpl: &str, ctx: &TemplateCtx) -> String {
    let dt = ctx.file.taken_at.and_then(meta::from_ts);
    let (y, m, d) = dt.map(|d| (format!("{:04}", d.year()), format!("{:02}", d.month()), format!("{:02}", d.day()))).unwrap_or_else(|| ("0000".into(), "00".into(), "00".into()));
    let kind = primary_kind(ctx);
    let kind_ko = KIND_KO.iter().find(|(k, _)| *k == kind).map(|(_, v)| *v).unwrap_or("기타");
    let source = ctx
        .labels
        .iter()
        .find_map(|l| l.strip_prefix("src:"))
        .map(|s| match s {
            "kakaotalk" => "카카오톡",
            "whatsapp" => "WhatsApp",
            "telegram" => "Telegram",
            "line" => "LINE",
            "instagram" => "Instagram",
            "download" => "다운로드",
            o => o,
        })
        .unwrap_or(if ctx.file.camera.is_some() { "카메라" } else { "기타" });
    let user_label = ctx.labels.iter().find(|l| !l.contains(':') && !matches!(l.as_str(), "screenshot" | "gps" | "edited")).map(String::as_str).unwrap_or("");
    let camera = ctx.file.camera.clone().unwrap_or_else(|| "Unknown".into());

    let rendered = tpl
        .replace("{year}", &y)
        .replace("{month}", &m)
        .replace("{day}", &d)
        .replace("{date}", &format!("{y}-{m}-{d}"))
        .replace("{kind}", kind_ko)
        .replace("{camera}", &camera)
        .replace("{source}", source)
        .replace("{label}", user_label)
        .replace("{trip}", ctx.trip.unwrap_or(""))
        .replace("{person}", ctx.person.unwrap_or(""));
    rendered
        .split(['/', '\\'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(sanitize)
        .collect::<Vec<_>>()
        .join("/")
}

fn target_name(f: &FileRow, rename: Rename) -> String {
    match rename {
        Rename::Keep => sanitize(&f.name),
        Rename::DatePrefix => {
            let prefix = f.taken_at.and_then(meta::from_ts).map(|d| d.format("%Y-%m-%d_%H%M%S").to_string());
            match prefix {
                // Don't stack prefixes when re-organizing an already organized library.
                Some(p) if !f.name.starts_with(&p) => sanitize(&format!("{p}_{}", f.name)),
                _ => sanitize(&f.name),
            }
        }
    }
}

fn with_suffix(name: &str, n: u32) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}_{n}.{ext}"),
        None => format!("{name}_{n}"),
    }
}

fn same_bytes(ha: Option<&str>, a: &Path, hb: Option<&str>, b: &Path) -> bool {
    let ha = ha.map(|h| Ok(h.to_string())).unwrap_or_else(|| journal::hash_file(a));
    let hb = hb.map(|h| Ok(h.to_string())).unwrap_or_else(|| journal::hash_file(b));
    matches!((ha, hb), (Ok(x), Ok(y)) if x == y)
}

/// Sidecar files: edits and metadata other programs keep beside a photo.
pub fn is_sidecar_name(lower: &str) -> bool {
    [".xmp", ".aae", ".thm", ".json"].iter().any(|e| lower.ends_with(e))
}

/// Sidecars of `src` among the names in its folder, each with where it goes next to `dst`
/// (renamed along when the photo was: `IMG_1.xmp` → `IMG_1 (2).xmp`).
fn sidecars(src: &Path, dst: &Path, names: &[String]) -> Vec<(PathBuf, PathBuf)> {
    let (Some(name), Some(dir), Some(dname), Some(ddir)) =
        (src.file_name().map(|n| n.to_string_lossy().into_owned()), src.parent(), dst.file_name().map(|n| n.to_string_lossy().into_owned()), dst.parent())
    else {
        return Vec::new();
    };
    let stem = name.rfind('.').map(|i| &name[..i]).unwrap_or(&name).to_string();
    let dstem = dname.rfind('.').map(|i| &dname[..i]).unwrap_or(&dname).to_string();
    let mut wanted: Vec<(String, String)> = Vec::new(); // (lower-cased candidate, destination name)
    // `IMG_1.xmp` beside `IMG_1.CR2` and `IMG_1.JPG` holds the RAW's edits: it goes with the RAW.
    const RAW: &[&str] = &["cr2", "cr3", "nef", "nrw", "arw", "srf", "sr2", "dng", "raf", "orf", "rw2", "pef", "srw", "x3f", "3fr", "iiq"];
    let is_raw = RAW.contains(&crate::media::ext_of(&name).as_str());
    let raw_beside = !is_raw && names.iter().any(|n| {
        let l = n.to_lowercase();
        l.strip_prefix(&format!("{}.", stem.to_lowercase())).is_some_and(|e| RAW.contains(&e))
    });
    for ext in ["xmp", "XMP", "aae", "AAE", "thm", "THM"] {
        if raw_beside && ext.eq_ignore_ascii_case("xmp") {
            continue;
        }
        wanted.push((format!("{stem}.{ext}").to_lowercase(), format!("{dstem}.{ext}")));
    }
    wanted.push((format!("{name}.xmp").to_lowercase(), format!("{dname}.xmp")));
    for p in meta::takeout_json_paths(src) {
        if let Some(n) = p.file_name().map(|n| n.to_string_lossy().into_owned()) {
            let renamed = if let Some(rest) = n.strip_prefix(&name) { format!("{dname}{rest}") } else { n.replacen(&stem, &dstem, 1) };
            wanted.push((n.to_lowercase(), renamed));
        }
    }
    let mut out: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut taken: HashSet<String> = HashSet::new();
    for (cand, to) in wanted {
        if let Some(real) = names.iter().find(|n| n.to_lowercase() == cand) {
            if taken.insert(real.to_lowercase()) {
                // Keep the real file's own ending (`.AAE` stays `.AAE`).
                let to = match (real.to_lowercase().strip_prefix(&stem.to_lowercase()), real.get(stem.len()..)) {
                    (Some(_), Some(tail)) if real.len() >= stem.len() => format!("{dstem}{tail}"),
                    _ => to,
                };
                out.push((dir.join(real), ddir.join(to)));
            }
        }
    }
    out
}

/// Files in a folder (lower-cased name -> size); empty if the folder does not exist yet.
fn list_dir(dir: &Path) -> HashMap<String, i64> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let md = e.metadata().ok()?;
                    md.is_file().then(|| (e.file_name().to_string_lossy().to_lowercase(), md.len() as i64))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn key(p: &Path) -> String {
    // NTFS/exFAT are case-insensitive; treat paths that differ only by case as the same.
    p.to_string_lossy().to_lowercase()
}

// ---------------------------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------------------------

pub fn plan(db: &Db, opts: &OrganizeOptions, stop: &StopFlag, progress: ProgressFn) -> Result<Plan> {
    if opts.target_dir.trim().is_empty() {
        bail!("정리할 대상 폴더를 선택하세요");
    }
    // Preview must not touch the disk: resolve the target without creating it.
    let target = volumes::canonical_lenient(Path::new(opts.target_dir.trim()))?;
    let tvol = volumes::identify_lenient(&target)?;

    let conn = db.conn()?;
    let clock = std::time::Instant::now();
    let mut timing: Vec<(String, u64)> = Vec::new();
    let mut lap = |name: &str, t: &mut std::time::Instant| {
        timing.push((name.to_string(), t.elapsed().as_millis() as u64));
        *t = std::time::Instant::now();
    };
    let mut t = std::time::Instant::now();
    volumes::refresh(&conn)?;
    let ids = db::query_ids(&conn, &opts.filter)?;
    let mut labels_of: HashMap<i64, Vec<String>> = HashMap::new();
    {
        let mut st = conn.prepare("SELECT file_id, label FROM labels")?;
        for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (id, l) = r?;
            labels_of.entry(id).or_default().push(l);
        }
    }

    let trips = crate::places::trip_titles(&conn)?;
    let persons = crate::people::folder_names(&conn)?;
    let library = opts.layout == "library";
    lap("목록", &mut t);
    // Planning reads nothing from the drives: fingerprints missing for same-size files are read
    // in the background (`identity::backfill_prehashes`); until then those files stay put.
    let mut placements = if library {
        let lib_vol: i64 = conn.query_row("SELECT id FROM volumes WHERE uid = ?", [&tvol.uid], |r| r.get(0)).unwrap_or(-1);
        let lib_root = volumes::rel_of(&tvol.root, &target).unwrap_or_default();
        crate::library::layout(&conn, lib_vol, &lib_root, &ids, &opts.library, &crate::library::load_rules(&conn)?)?
    } else {
        HashMap::new()
    };
    // Folders the user merged: everything planned into one goes into the other.
    if library {
        let merges = crate::library::load_merges(&conn)?;
        if !merges.is_empty() {
            for p in placements.values_mut() {
                if !p.rel_dir.is_empty() {
                    p.rel_dir = crate::library::merged(&p.rel_dir, &merges);
                }
            }
        }
    }
    lap("사본·배치", &mut t);
    // Copies the user protected (♥, original, keep folder) stay where they are: not counted as
    // work, so a year with only those left shows as done.
    let protected: HashSet<i64> = {
        let rm: Vec<i64> = placements.iter().filter(|(_, p)| p.act == crate::library::Act::Remove).map(|(id, _)| *id).collect();
        if rm.is_empty() { HashSet::new() } else { crate::protect::check(&conn, &rm)?.into_iter().map(|b| b.id).collect() }
    };
    lap("보호 확인", &mut t);
    let mut events: HashSet<String> = HashSet::new();
    let mut items = Vec::with_capacity(ids.len());
    let mut summary = PlanSummary::default();
    // Fingerprints (size + first 64 KiB) decide "same file" while planning; nothing is read.
    let prehash: HashMap<i64, String> = {
        let mut st = conn.prepare("SELECT id, COALESCE(prehash, hash) FROM files WHERE COALESCE(prehash, hash) IS NOT NULL AND COALESCE(prehash, hash) != ''")?;
        let m = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<HashMap<i64, String>>>()?;
        m
    };
    let same = |a: (i64, Option<&String>), b: (i64, Option<&String>)| a.0 == b.0 && a.1.is_some() && a.1 == b.1;
    let uids: HashMap<i64, String> = {
        let mut st = conn.prepare("SELECT id, uid FROM volumes")?;
        let m = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<HashMap<i64, String>>>()?;
        m
    };
    // dst key -> (size, src path, fingerprint) of the file that claimed it first in this plan.
    let mut claimed: HashMap<String, (i64, PathBuf, Option<String>)> = HashMap::new();
    let mut folders: HashSet<String> = HashSet::new();
    // What already sits in each destination folder (lower-cased name -> size, fingerprint).
    // Library: from the index (the library folder is a source), so no folder is listed on disk;
    // a file that is there but not indexed makes that one move fail safely (nothing overwritten).
    let mut on_disk: HashMap<String, HashMap<String, (i64, Option<String>)>> = HashMap::new();
    if library {
        let tvol_id: Option<i64> = conn.query_row("SELECT id FROM volumes WHERE uid = ?", [&tvol.uid], |r| r.get(0)).ok();
        if let Some(vid) = tvol_id {
            let mut st = conn.prepare("SELECT v.mount, f.rel_path, f.size, f.id FROM files f JOIN volumes v ON v.id = f.volume_id WHERE f.volume_id = ? AND f.missing = 0 AND f.quarantined = 0")?;
            let rows = st.query_map([vid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)))?;
            for r in rows {
                let (mount, rel, size, id) = r?;
                let abs = volumes::join_rel(Path::new(&mount), &rel);
                let (Some(dir), Some(name)) = (abs.parent(), abs.file_name()) else { continue };
                on_disk.entry(key(dir)).or_default().insert(name.to_string_lossy().to_lowercase(), (size, prehash.get(&id).cloned()));
            }
        }
    }
    let total = ids.len() as u64;
    let empty = Vec::new();
    let rows = db::all_files(&conn)?;
    for (i, id) in ids.iter().enumerate() {
        if stop.is_stopped() {
            bail!("cancelled");
        }
        if i % 500 == 0 {
            progress(Progress::new("organize:plan", "plan", i as u64, total, ""));
        }
        let Some(f) = rows.get(id) else { continue };
        let labels = labels_of.get(id).unwrap_or(&empty);
        summary.total += 1;
        let src = f.abs_path();
        let uncertain = meta::confidence(&f.date_source) == "low";
        // Library: what lands in 「_날짜확인필요」 (an undated scan in a dated event folder does not).
        let to_check = match placements.get(id) {
            Some(p) if library => matches!(p.act, crate::library::Act::Move | crate::library::Act::Copy) && p.rel_dir.split('/').any(|s| s == "_날짜확인필요"),
            _ => uncertain,
        };
        if to_check {
            summary.uncertain_dates += 1;
        }
        let ctx = TemplateCtx { file: &f, labels, trip: trips.get(id).map(String::as_str), person: persons.get(id).map(String::as_str) };
        let place = placements.get(id);
        if library {
            let Some(p) = place else { continue };
            if matches!(p.act, crate::library::Act::Leave | crate::library::Act::Remove) {
                let remove = p.act == crate::library::Act::Remove && !protected.contains(id);
                if remove {
                    summary.remove += 1;
                    summary.remove_bytes += f.size;
                } else if p.tag == "unverified" {
                    summary.unverified += 1;
                } else if p.tag == "excluded" {
                    summary.excluded += 1;
                } else {
                    summary.leave += 1;
                }
                items.push(PlanItem {
                    file_id: f.id,
                    src: src.to_string_lossy().into_owned(),
                    dst: String::new(),
                    action: if remove { Action::Remove } else { Action::Leave },
                    renamed: false,
                    cross_volume: false,
                    size: f.size,
                    group: String::new(),
                    tag: if p.act == crate::library::Act::Remove && !remove { "protected".into() } else { p.tag.into() },
                    event: None,
                    from: p.from_dir.clone(),
                    kept: p.kept,
                    vol: f.volume_id,
                });
                continue;
            }
        }
        let rel_dir = match (place, &opts.uncertain_folder, uncertain) {
            (Some(p), _, _) => p.rel_dir.clone(),
            (None, Some(u), true) if !u.trim().is_empty() => format!("{}/{}", sanitize(u.trim()), render_template(&opts.template, &ctx)),
            _ => render_template(&opts.template, &ctx),
        };
        let dir = volumes::join_rel(&target, &rel_dir);
        let base_name = target_name(&f, opts.rename);
        let copy = match place {
            Some(p) => p.act == crate::library::Act::Copy,
            None => opts.mode == Mode::Copy,
        };
        let mut item = PlanItem {
            file_id: f.id,
            src: src.to_string_lossy().into_owned(),
            dst: String::new(),
            action: if copy { Action::Copy } else { Action::Move },
            renamed: false,
            cross_volume: uids.get(&f.volume_id).is_none_or(|u| *u != tvol.uid),
            size: f.size,
            group: rel_dir.clone(),
            tag: place.map(|p| p.tag.to_string()).unwrap_or_default(),
            event: place.and_then(|p| p.event.clone()),
            from: place.map(|p| p.from_dir.clone()).unwrap_or_default(),
            kept: None,
            vol: f.volume_id,
        };
        if !f.online {
            item.action = Action::SkipOffline;
            item.dst = dir.join(&base_name).to_string_lossy().into_owned();
            summary.skip_offline += 1;
            items.push(item);
            continue;
        }

        let mut n = 0;
        let dst = loop {
            let name = if n == 0 { base_name.clone() } else { with_suffix(&base_name, n) };
            let cand = dir.join(&name);
            let k = key(&cand);
            if key(&src) == k {
                item.action = Action::InPlace;
                break cand;
            }
            let mine = (f.size, prehash.get(&f.id));
            if let Some((csize, csrc, chash)) = claimed.get(&k) {
                // Two identical files from different drives: keep one, skip the other.
                let dup = if library {
                    same((*csize, chash.as_ref()), mine)
                } else {
                    *csize == f.size && same_bytes(chash.as_deref(), csrc, f.hash.as_deref(), &src)
                };
                if dup {
                    item.action = Action::SkipDuplicate;
                    break cand;
                }
                n += 1;
                continue;
            }
            let listing = on_disk.entry(key(&dir)).or_insert_with(|| {
                if library {
                    HashMap::new()
                } else {
                    list_dir(&dir).into_iter().map(|(k, s)| (k, (s, None))).collect()
                }
            });
            if let Some((size, ph)) = listing.get(&name.to_lowercase()).cloned() {
                // Same file already there? Then this one is a duplicate: skip it (it stays where
                // it is, so nothing is lost if the fingerprints only look alike).
                let dup = if library { same((size, ph.as_ref()), mine) } else { size == f.size && same_bytes(None, &cand, f.hash.as_deref(), &src) };
                if dup {
                    item.action = Action::SkipDuplicate;
                    break cand;
                }
                n += 1;
                continue;
            }
            break cand;
        };
        item.renamed = n > 0;
        if item.action != Action::SkipDuplicate {
            let fp = if library { prehash.get(&f.id).cloned() } else { f.hash.clone() };
            claimed.insert(key(&dst), (f.size, src.clone(), fp));
        }
        item.dst = dst.to_string_lossy().into_owned();
        match item.action {
            Action::Move => summary.moves += 1,
            Action::Copy => {
                summary.copies += 1;
                summary.copy_bytes += f.size;
            }
            Action::InPlace => summary.in_place += 1,
            Action::SkipDuplicate => summary.skip_duplicate += 1,
            Action::SkipOffline => summary.skip_offline += 1,
            Action::Leave | Action::Remove => {}
        }
        if let Some(p) = place {
            match p.tag {
                "received" => summary.received += 1,
                "raw" | "live" | "edited" | "version" => summary.paired += 1,
                _ => {}
            }
            if p.rel_dir.starts_with("큰 동영상/") {
                summary.big_videos += 1;
            }
            if let Some(e) = &p.event {
                events.insert(format!("{}|{e}", p.rel_dir));
            }
        }
        if matches!(item.action, Action::Move | Action::Copy) {
            summary.bytes += f.size;
            if item.renamed {
                summary.renamed += 1;
            }
            if item.cross_volume {
                summary.cross_volume += 1;
            }
            folders.insert(key(&dir));
        }
        items.push(item);
    }
    summary.folders = folders.len() as u64;
    summary.events = events.len() as u64;
    lap("목적지 확인", &mut t);
    summary.timing = timing;
    summary.timing.push(("전체".into(), clock.elapsed().as_millis() as u64));
    progress(Progress::new("organize:plan", "done", total, total, ""));
    let mut plan = Plan { id: uuid::Uuid::new_v4().simple().to_string(), options: opts.clone(), items, summary, years: HashMap::new() };
    if library {
        plan.years = compute_years(&plan);
    }
    Ok(plan)
}

/// One destination folder of a library plan, for the preview tree.
#[derive(Clone, Debug, Serialize)]
pub struct TreeNode {
    /// Folder inside the library (`2024/2024-06/하와이 여행`).
    pub group: String,
    pub event: Option<String>,
    pub tag: String,
    pub count: u64,
    pub bytes: i64,
    pub moves: u64,
    pub copies: u64,
    pub in_place: u64,
    /// The event's source folder (drive + volume-relative path) for renaming / skipping it.
    pub vol: i64,
    pub from_dir: String,
    /// Where its files come from now (folder, count), most first.
    pub sources: Vec<(String, u64)>,
}

/// The plan as destination folders (files that land in the library), sorted by path.
pub fn plan_tree(plan: &Plan) -> Vec<TreeNode> {
    let mut nodes: HashMap<String, TreeNode> = HashMap::new();
    let mut srcs: HashMap<String, HashMap<String, u64>> = HashMap::new();
    for it in plan.items.iter().filter(|i| matches!(i.action, Action::Move | Action::Copy | Action::InPlace | Action::SkipDuplicate)) {
        let n = nodes.entry(it.group.clone()).or_insert_with(|| TreeNode {
            group: it.group.clone(),
            event: it.event.clone(),
            tag: it.tag.clone(),
            count: 0,
            bytes: 0,
            moves: 0,
            copies: 0,
            in_place: 0,
            vol: it.vol,
            from_dir: it.from.clone(),
            sources: Vec::new(),
        });
        // The folder's own kind: a RAW / Live Photo / edited version only travels with its photo.
        let follower = |t: &str| matches!(t, "raw" | "live" | "edited" | "version" | "");
        if follower(&n.tag) && !follower(&it.tag) {
            n.tag = it.tag.clone();
        }
        if n.event.is_none() && it.event.is_some() {
            n.event = it.event.clone();
            n.from_dir = it.from.clone();
            n.vol = it.vol;
        }
        n.count += 1;
        n.bytes += it.size;
        match it.action {
            Action::Move => n.moves += 1,
            Action::Copy => n.copies += 1,
            _ => n.in_place += 1,
        }
        if matches!(it.action, Action::Move | Action::Copy) {
            let from = Path::new(&it.src).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            *srcs.entry(it.group.clone()).or_default().entry(from).or_default() += 1;
        }
    }
    let mut out: Vec<TreeNode> = nodes
        .into_values()
        .map(|mut n| {
            let mut s: Vec<(String, u64)> = srcs.remove(&n.group).unwrap_or_default().into_iter().collect();
            s.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            s.truncate(5);
            n.sources = s;
            n
        })
        .collect();
    out.sort_by(|a, b| a.group.cmp(&b.group));
    out
}

/// Only the given destination folders of a plan (to try one folder first), with the copies on
/// the library drive whose kept twin is among them.
pub fn subset(plan: &Plan, groups: &[String]) -> Plan {
    let want: HashSet<&str> = groups.iter().map(String::as_str).collect();
    let mut items: Vec<PlanItem> = plan.items.iter().filter(|i| matches!(i.action, Action::Move | Action::Copy) && want.contains(i.group.as_str())).cloned().collect();
    let moved: HashSet<i64> = items.iter().map(|i| i.file_id).collect();
    items.extend(plan.items.iter().filter(|i| i.action == Action::Remove && i.kept.is_some_and(|k| moved.contains(&k))).cloned());
    part_of(plan, items)
}

fn part_of(plan: &Plan, items: Vec<PlanItem>) -> Plan {
    let mut summary = PlanSummary::default();
    for it in &items {
        match it.action {
            Action::Move => summary.moves += 1,
            Action::Copy => {
                summary.copies += 1;
                summary.copy_bytes += it.size;
            }
            Action::Remove => {
                summary.remove += 1;
                summary.remove_bytes += it.size;
            }
            _ => {}
        }
    }
    Plan { id: plan.id.clone(), options: plan.options.clone(), items, summary, years: plan.years.clone() }
}

// ---------------------------------------------------------------------------------------------
// Year by year ("한 해씩"): the library plan split by the year a file lands in
// ---------------------------------------------------------------------------------------------

/// The year a library folder belongs to: `2015/…`, `받은 사진/2015/…`, `큰 동영상/2015/…` → "2015";
/// before 2010 → "before2010"; `_날짜확인필요` → "undated".
pub fn year_key(group: &str) -> String {
    let mut segs = group.split('/');
    let mut s = segs.next().unwrap_or("");
    if s == "받은 사진" || s == "큰 동영상" {
        s = segs.next().unwrap_or("");
    }
    match s.parse::<i32>() {
        Ok(y) if s.len() == 4 && y < 2010 => "before2010".into(),
        Ok(_) if s.len() == 4 => s.to_string(),
        _ => "undated".into(),
    }
}

pub fn year_label(key: &str) -> String {
    match key {
        "before2010" => "2010년 이전".into(),
        "undated" => "날짜 모름".into(),
        y => format!("{y}년"),
    }
}

fn year_rank(key: &str) -> (u8, String) {
    match key {
        "before2010" => (0, String::new()),
        "undated" => (2, String::new()),
        y => (1, y.to_string()),
    }
}

/// Year of each planned file: files landing in the library by their folder, copies to remove by
/// the file kept instead (so a copy shows up with its original, wherever it lies now).
fn item_years(plan: &Plan) -> std::borrow::Cow<'_, HashMap<i64, String>> {
    if plan.years.is_empty() {
        std::borrow::Cow::Owned(compute_years(plan))
    } else {
        std::borrow::Cow::Borrowed(&plan.years)
    }
}

fn compute_years(plan: &Plan) -> HashMap<i64, String> {
    let mut years: HashMap<i64, String> = plan
        .items
        .iter()
        .filter(|i| matches!(i.action, Action::Move | Action::Copy | Action::InPlace | Action::SkipDuplicate))
        .map(|i| (i.file_id, year_key(&i.group)))
        .collect();
    for it in plan.items.iter().filter(|i| i.action == Action::Remove) {
        let y = it.kept.and_then(|k| years.get(&k).cloned()).unwrap_or_else(|| "undated".into());
        years.insert(it.file_id, y);
    }
    years
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct YearBucket {
    pub key: String,
    pub label: String,
    /// Files the library keeps for this year (moved, copied in, or already in place).
    pub keep: u64,
    pub keep_bytes: i64,
    pub moves: u64,
    pub copies: u64,
    pub copy_bytes: i64,
    pub in_place: u64,
    /// Identical copies on the library drive, deleted after a byte-for-byte check.
    pub remove: u64,
    pub remove_bytes: i64,
    /// Resized / recompressed versions: quarantined for a look (not deleted right away).
    pub versions: u64,
    pub versions_bytes: i64,
    /// Nothing left to do this year.
    pub done: bool,
}

pub fn year_buckets(plan: &Plan) -> Vec<YearBucket> {
    let years = item_years(plan);
    let mut out: HashMap<String, YearBucket> = HashMap::new();
    for it in &plan.items {
        let Some(y) = years.get(&it.file_id) else { continue };
        let b = out.entry(y.clone()).or_insert_with(|| YearBucket { key: y.clone(), label: year_label(y), ..Default::default() });
        match it.action {
            Action::Move | Action::Copy | Action::InPlace | Action::SkipDuplicate => {
                b.keep += 1;
                b.keep_bytes += it.size;
                match it.action {
                    Action::Move => b.moves += 1,
                    Action::Copy => {
                        b.copies += 1;
                        b.copy_bytes += it.size;
                    }
                    _ => b.in_place += 1,
                }
            }
            Action::Remove if it.tag == "version" => {
                b.versions += 1;
                b.versions_bytes += it.size;
            }
            Action::Remove => {
                b.remove += 1;
                b.remove_bytes += it.size;
            }
            _ => {}
        }
    }
    let mut v: Vec<YearBucket> = out
        .into_values()
        .map(|mut b| {
            b.done = b.moves + b.copies + b.remove + b.versions == 0;
            b
        })
        .collect();
    v.sort_by_key(|b| year_rank(&b.key));
    v
}

/// One year of the plan: what lands in that year's folders and the copies of those files.
pub fn subset_year(plan: &Plan, key: &str) -> Plan {
    let years = item_years(plan);
    let items: Vec<PlanItem> = plan
        .items
        .iter()
        .filter(|i| matches!(i.action, Action::Move | Action::Copy | Action::Remove) && years.get(&i.file_id).is_some_and(|y| y == key))
        .cloned()
        .collect();
    part_of(plan, items)
}

/// Every file of one year (landing in its folders, or a copy of one of those).
pub fn year_files(plan: &Plan, key: &str) -> HashSet<i64> {
    item_years(plan).iter().filter(|(_, y)| y.as_str() == key).map(|(id, _)| *id).collect()
}

/// The library drive of a plan (the drive files are moved within).
pub fn lib_vol(plan: &Plan) -> Option<i64> {
    plan.items.iter().find(|i| !i.cross_volume && matches!(i.action, Action::Move | Action::InPlace | Action::Remove)).map(|i| i.vol)
}

/// Copies to remove in one year, by the folder they are in now.
#[derive(Clone, Debug, Serialize)]
pub struct CopyFolder {
    pub dir: String,
    pub count: u64,
    pub bytes: i64,
    pub versions: u64,
    /// 복사본: same name as the original kept (the rest are 사본: same content, another name).
    pub same_name: u64,
    /// Library folders their originals go to (most first, at most 3).
    pub kept_in: Vec<(String, u64)>,
}

/// A copy and the original kept instead.
#[derive(Clone, Debug, Serialize)]
pub struct CopyPair {
    pub file_id: i64,
    pub src: String,
    pub size: i64,
    /// "copy" (identical) or "version" (resized / recompressed).
    pub tag: String,
    pub kept_id: i64,
    /// Where the original is now, and where it goes in the library.
    pub kept_src: String,
    pub kept_dst: String,
}

/// 복사본 (same file name, ignoring case) or 사본 (another name).
pub fn same_name(a: &str, b: &str) -> bool {
    let name = |p: &str| Path::new(p).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    name(a) == name(b)
}

fn parent_of(p: &str) -> String {
    Path::new(p).parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default()
}

pub fn year_copies(plan: &Plan, key: &str) -> Vec<CopyFolder> {
    let part = subset_year(plan, key);
    let groups: HashMap<i64, &str> = plan.items.iter().map(|i| (i.file_id, i.group.as_str())).collect();
    let srcs: HashMap<i64, &str> = plan.items.iter().map(|i| (i.file_id, i.src.as_str())).collect();
    let mut by_dir: HashMap<String, (CopyFolder, HashMap<String, u64>)> = HashMap::new();
    for it in part.items.iter().filter(|i| i.action == Action::Remove) {
        let dir = parent_of(&it.src);
        let e = by_dir
            .entry(dir.clone())
            .or_insert_with(|| (CopyFolder { dir, count: 0, bytes: 0, versions: 0, same_name: 0, kept_in: Vec::new() }, HashMap::new()));
        e.0.count += 1;
        if it.kept.and_then(|k| srcs.get(&k)).is_some_and(|k| same_name(k, &it.src)) {
            e.0.same_name += 1;
        }
        e.0.bytes += it.size;
        if it.tag == "version" {
            e.0.versions += 1;
        }
        if let Some(g) = it.kept.and_then(|k| groups.get(&k)) {
            *e.1.entry(g.to_string()).or_default() += 1;
        }
    }
    let mut out: Vec<CopyFolder> = by_dir
        .into_values()
        .map(|(mut f, kept)| {
            let mut k: Vec<(String, u64)> = kept.into_iter().collect();
            k.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            k.truncate(3);
            f.kept_in = k;
            f
        })
        .collect();
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.dir.cmp(&b.dir)));
    out
}

pub fn year_copy_pairs(plan: &Plan, key: &str, dir: &str, offset: usize, limit: usize) -> Vec<CopyPair> {
    let part = subset_year(plan, key);
    let by_id: HashMap<i64, &PlanItem> = plan.items.iter().map(|i| (i.file_id, i)).collect();
    part.items
        .iter()
        .filter(|i| i.action == Action::Remove && parent_of(&i.src) == dir)
        .skip(offset)
        .take(limit)
        .filter_map(|i| {
            let k = by_id.get(&i.kept?)?;
            Some(CopyPair {
                file_id: i.file_id,
                src: i.src.clone(),
                size: i.size,
                tag: i.tag.clone(),
                kept_id: k.file_id,
                kept_src: k.src.clone(),
                kept_dst: if k.dst.is_empty() { k.src.clone() } else { k.dst.clone() },
            })
        })
        .collect()
}

/// Files planned into the given library folders (for the preview of a selection).
pub fn group_items(plan: &Plan, groups: &[String], offset: usize, limit: usize) -> Vec<KeepItem> {
    let want: HashSet<&str> = groups.iter().map(String::as_str).collect();
    let mut copies: HashMap<i64, u32> = HashMap::new();
    for it in plan.items.iter().filter(|i| i.action == Action::Remove) {
        if let Some(k) = it.kept {
            *copies.entry(k).or_default() += 1;
        }
    }
    let mut v: Vec<&PlanItem> = plan
        .items
        .iter()
        .filter(|i| matches!(i.action, Action::Move | Action::Copy | Action::InPlace | Action::SkipDuplicate) && want.contains(i.group.as_str()))
        .collect();
    v.sort_by(|a, b| a.group.cmp(&b.group).then(a.dst.cmp(&b.dst)));
    v.into_iter()
        .skip(offset)
        .take(limit)
        .map(|i| KeepItem { file_id: i.file_id, group: i.group.clone(), action: i.action, copies: copies.get(&i.file_id).copied().unwrap_or(0), size: i.size })
        .collect()
}

/// What deleting library folders for good means: the files on the library drive planned into
/// them and their copies there (deleted), and the photos only on other drives that would have
/// been copied in (left there, not brought back).
#[derive(Clone, Debug, Default, Serialize)]
pub struct DeleteTargets {
    /// On the library drive: deleted.
    pub ids: Vec<i64>,
    pub bytes: i64,
    /// Photos planned to be copied from other drives: not copied (and not later either).
    pub not_copied: u64,
    /// Ids whose content is excluded once deleted: the copies-in (left on their drive).
    pub copy_in_ids: Vec<i64>,
}

pub fn delete_targets(plan: &Plan, groups: &[String]) -> DeleteTargets {
    let want: HashSet<&str> = groups.iter().map(String::as_str).collect();
    let mut t = DeleteTargets::default();
    let mut on_lib: HashSet<i64> = HashSet::new();
    for it in plan.items.iter().filter(|i| want.contains(i.group.as_str())) {
        match it.action {
            Action::Copy => {
                t.not_copied += 1;
                t.copy_in_ids.push(it.file_id);
            }
            Action::Move | Action::InPlace | Action::SkipDuplicate if !it.cross_volume => {
                on_lib.insert(it.file_id);
                t.ids.push(it.file_id);
                t.bytes += it.size;
            }
            _ => {}
        }
    }
    // Their copies on the library drive go with them.
    for it in plan.items.iter().filter(|i| i.action == Action::Remove && i.kept.is_some_and(|k| on_lib.contains(&k))) {
        t.ids.push(it.file_id);
        t.bytes += it.size;
    }
    t
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FolderDeleteReport {
    pub batch_id: String,
    pub deleted: u64,
    pub bytes: i64,
    /// ♥ / original / keep-folder files left in place (when asked to keep protected files).
    pub kept_protected: u64,
    /// Photos on other drives no longer copied in (and not brought back later).
    pub not_copied: u64,
    pub failed: u64,
    pub errors: Vec<String>,
}

/// Delete library folders for good (see `delete_targets`). With `keep_protected`, files marked ♥
/// / original / in a keep folder stay ("the last copy" does not count here: deleting the photos
/// is the point). Their contents are remembered so copies on other drives are not copied back in.
pub fn delete_folders(db: &Db, plan: &Plan, groups: &[String], keep_protected: bool, stop: &StopFlag, progress: ProgressFn) -> Result<FolderDeleteReport> {
    let t = delete_targets(plan, groups);
    let label = format!("폴더 {}개 영구 삭제", groups.len());
    let mut rep = delete_for_good(db, &t.ids, &t.copy_in_ids, keep_protected, &label, stop, progress)?;
    rep.not_copied = t.not_copied;
    Ok(rep)
}

/// Delete files for good (no quarantine) and remember their contents, so copies of them on other
/// drives are not brought into the library again; `copy_in_ids` are photos on other drives that
/// are only to be kept out (not deleted). With `keep_protected`, ♥ / original / keep-folder files
/// stay (the "last copy" protection does not apply: deleting the photo is the point).
pub fn delete_for_good(
    db: &Db,
    ids: &[i64],
    copy_in_ids: &[i64],
    keep_protected: bool,
    label: &str,
    stop: &StopFlag,
    progress: ProgressFn,
) -> Result<FolderDeleteReport> {
    let conn = db.conn()?;
    let mut ids: Vec<i64> = ids.to_vec();
    let mut rep = FolderDeleteReport::default();
    let protected: HashSet<i64> = if keep_protected {
        crate::protect::check(&conn, &ids)?.into_iter().filter(|b| b.reason != "last_copy").map(|b| b.id).collect()
    } else {
        HashSet::new()
    };
    rep.kept_protected = protected.len() as u64;
    ids.retain(|id| !protected.contains(id));
    // Every photo involved needs a fingerprint to be remembered (a photo with no same-size twin
    // was never fingerprinted): read the missing ones now — a few, chosen by the user.
    for id in ids.iter().chain(copy_in_ids.iter()) {
        let has: bool = conn.query_row("SELECT COALESCE(prehash, hash) IS NOT NULL FROM files WHERE id = ?", [*id], |r| r.get(0)).unwrap_or(true);
        if !has {
            if let Some(f) = db::get_file(&conn, *id)? {
                if let Ok(h) = journal::prehash_file(&f.abs_path(), f.size as u64) {
                    conn.execute("UPDATE files SET prehash = ? WHERE id = ?", rusqlite::params![h, id])?;
                }
            }
        }
    }
    let key_of = |id: i64| -> Option<String> {
        conn.query_row("SELECT COALESCE(prehash, hash) FROM files WHERE id = ?", [id], |r| r.get::<_, Option<String>>(0)).ok().flatten()
    };
    let kept_keys: HashSet<String> = protected.iter().filter_map(|id| key_of(*id)).collect();
    // Fingerprints read before deleting (the rows go with the files).
    let mut keys: Vec<String> = ids.iter().chain(copy_in_ids.iter()).filter_map(|id| key_of(*id)).filter(|k| !kept_keys.contains(k)).collect();
    if !ids.is_empty() {
        let d = journal::delete_files(db, &ids, true, Some(label), stop, progress)?;
        rep.batch_id = d.batch_id.clone();
        rep.failed = d.failed;
        rep.errors = d.errors;
        if !d.batch_id.is_empty() {
            let p = journal::purge_batch_with(&conn, &d.batch_id, stop, progress)?;
            rep.deleted = p.deleted;
            rep.bytes = p.bytes;
            rep.failed += p.failed + p.kept;
            rep.errors.extend(p.errors);
        }
    }
    // Only what is really gone is excluded (a file that could not be deleted keeps its content in).
    let still_there: HashSet<String> = ids
        .iter()
        .filter(|id| conn.query_row("SELECT 1 FROM files WHERE id = ? AND quarantined = 0", [**id], |_| Ok(())).is_ok())
        .filter_map(|id| key_of(*id))
        .collect();
    keys.retain(|k| !still_there.contains(k));
    let mut all = crate::library::load_excluded(&conn)?;
    let have: HashSet<String> = all.iter().cloned().collect();
    keys.sort();
    keys.dedup();
    all.extend(keys.into_iter().filter(|k| !have.contains(k)));
    crate::library::save_excluded(&conn, &all)?;
    Ok(rep)
}

/// Files the library keeps for one year, by library folder (for the thumbnails), with how many
/// copies of each go.
#[derive(Clone, Debug, Serialize)]
pub struct KeepItem {
    pub file_id: i64,
    pub group: String,
    pub action: Action,
    pub copies: u32,
    pub size: i64,
}

pub fn year_keep(plan: &Plan, key: &str, offset: usize, limit: usize) -> Vec<KeepItem> {
    let mut copies: HashMap<i64, u32> = HashMap::new();
    for it in plan.items.iter().filter(|i| i.action == Action::Remove) {
        if let Some(k) = it.kept {
            *copies.entry(k).or_default() += 1;
        }
    }
    let mut v: Vec<&PlanItem> = plan
        .items
        .iter()
        .filter(|i| matches!(i.action, Action::Move | Action::Copy | Action::InPlace | Action::SkipDuplicate) && year_key(&i.group) == key)
        .collect();
    v.sort_by(|a, b| a.group.cmp(&b.group).then(a.dst.cmp(&b.dst)));
    v.into_iter()
        .skip(offset)
        .take(limit)
        .map(|i| KeepItem { file_id: i.file_id, group: i.group.clone(), action: i.action, copies: copies.get(&i.file_id).copied().unwrap_or(0), size: i.size })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Execute
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize)]
pub struct ExecReport {
    pub batch_id: String,
    pub done: u64,
    pub failed: u64,
    pub bytes: i64,
    /// Sidecar files (.xmp / .aae / .thm / .json) moved or copied with their photo.
    pub sidecars: u64,
    /// Library: copies on the library drive taken away (quarantined), and ones kept (twin not in
    /// place, or protected).
    pub removed: u64,
    pub removed_bytes: i64,
    pub remove_skipped: u64,
    /// History entries of the copies (identical, deleted when `purge`) and of the resized /
    /// recompressed versions (quarantined for a look). Empty when there were none.
    pub copies_batch: String,
    pub versions_batch: String,
    pub versions: u64,
    /// Identical copies deleted for good after a byte-for-byte check, and ones left in the
    /// quarantine because the check failed.
    pub purged: u64,
    pub purged_bytes: i64,
    pub purge_kept: u64,
    /// Copies from other drives not made for lack of room (planning again lists them).
    pub space_skipped: u64,
    pub space_skipped_bytes: i64,
    /// Photos on the delete list: deleted for good, kept (protected or failed), history entry.
    pub dropped: u64,
    pub dropped_bytes: i64,
    pub drop_kept: u64,
    pub drop_batch: String,
    /// Photos on the delete list that are on other drives: left there, not brought in.
    pub not_brought: u64,
    /// Other files (documents…) left in the folders photos moved out of.
    pub leftover_files: u64,
    pub errors: Vec<String>,
    pub cancelled: bool,
}

fn human_bytes(b: i64) -> String {
    let b = b.max(0) as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else {
        format!("{:.0} KB", b / 1e3)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ExecOptions {
    /// Delete identical copies for good (after comparing each with its kept twin), so the room
    /// is free right away.
    pub purge: bool,
    /// Names the history entries ("2015년").
    pub label: Option<String>,
    /// Photos the user put on the delete list (similar photos not kept, picked by hand): deleted
    /// for good before moving (♥ / original / keep folder stay), not brought back from other drives.
    pub drop_ids: Vec<i64>,
    /// Review decisions to remember (kept photos are not suggested again).
    pub decisions: Vec<crate::triage::Decision>,
}

pub fn execute(db: &Db, plan: &Plan, stop: &StopFlag, progress: ProgressFn) -> Result<ExecReport> {
    execute_with(db, plan, &ExecOptions::default(), stop, progress)
}

/// Run a plan. Library plans go in this order, so the room copies need is made first:
/// 1. copies on the library drive are taken away (their own history entry; deleted for good
///    after a byte check when `purge`), 2. files on the library drive are moved (renames: no
///    room needed), 3. files only on other drives are copied in, as far as the room goes.
pub fn execute_with(db: &Db, plan: &Plan, xo: &ExecOptions, stop: &StopFlag, progress: ProgressFn) -> Result<ExecReport> {
    let conn = db.conn()?;
    volumes::refresh(&conn)?;
    std::fs::create_dir_all(plan.options.target_dir.trim())?;
    let target = volumes::canonical(Path::new(plan.options.target_dir.trim()))?;
    // The library folder becomes a source, so it stays indexed from now on.
    volumes::add_source(&conn, &target)?;
    let tvol = volumes::identify(&target)?;
    let tvol_id = volumes::upsert(&conn, &tvol)?;

    // Moves first (renames on the library drive), then copies from other drives.
    let mut work: Vec<&PlanItem> = plan.items.iter().filter(|i| matches!(i.action, Action::Move | Action::Copy)).collect();
    work.sort_by_key(|i| i.action == Action::Copy);
    let library = plan.options.layout == "library";
    let prefix = xo.label.as_deref().map(|l| format!("{l} ")).unwrap_or_default();
    let what = if library {
        let n = |a: Action| work.iter().filter(|i| i.action == a).count();
        let parts: Vec<String> = [("이동", n(Action::Move)), ("가져오기", n(Action::Copy))]
            .iter()
            .filter(|(_, c)| *c > 0)
            .map(|(w, c)| format!("{w} {c}"))
            .collect();
        format!("{prefix}라이브러리로 모으기 → {} ({})", target.display(), parts.join(" · "))
    } else {
        let verb = if plan.options.mode == Mode::Move { "이동" } else { "복사" };
        format!("{} → {} ({}개 {verb})", plan.options.template, target.display(), work.len())
    };
    let mut rep = ExecReport::default();
    let mut touched_dirs: HashSet<PathBuf> = HashSet::new();
    let source_roots: Vec<PathBuf> = db::list_sources(&conn)?.into_iter().map(|s| PathBuf::from(s.abs_path)).collect();

    // 1. Copies on the library drive, while their kept twin is surely in place (before anything
    //    moves). Protected files (♥, original, keep folders) stay.
    let removes: Vec<&PlanItem> = plan.items.iter().filter(|i| i.action == Action::Remove).collect();
    if !removes.is_empty() {
        let ids: Vec<i64> = removes.iter().map(|i| i.file_id).collect();
        let protected: HashSet<i64> = crate::protect::check(&conn, &ids)?.into_iter().map(|b| b.id).collect();
        let copies_batch = journal::new_batch(&conn, "copies", &format!("{prefix}사본 정리"))?;
        let versions_batch = journal::new_batch(&conn, "copies", &format!("{prefix}축소본 격리 (확인 후 비우기)"))?;
        let (mut copy_n, mut copy_bytes, mut ver_n, mut ver_bytes) = (0u64, 0i64, 0u64, 0i64);
        let total = removes.len() as u64;
        for (i, it) in removes.into_iter().enumerate() {
            if stop.is_stopped() {
                rep.cancelled = true;
                break;
            }
            if i % 20 == 0 {
                progress(Progress::new("organize:run", "copies", i as u64, total, it.src.clone()));
            }
            let kept_ok = it
                .kept
                .and_then(|k| db::get_file(&conn, k).ok().flatten())
                .is_some_and(|k| !k.missing && !k.quarantined && k.abs_path().is_file());
            if !kept_ok || protected.contains(&it.file_id) {
                rep.remove_skipped += 1;
                continue;
            }
            let Some(f) = db::get_file(&conn, it.file_id)? else { continue };
            if f.quarantined || f.missing || !f.online {
                continue;
            }
            let version = it.tag == "version";
            let res = match (version, it.kept) {
                (false, Some(k)) => journal::quarantine_copy(&conn, &copies_batch, &f, k),
                _ => journal::quarantine(&conn, &versions_batch, &f),
            };
            match res {
                Ok(()) => {
                    rep.removed += 1;
                    rep.removed_bytes += f.size;
                    if version {
                        ver_n += 1;
                        ver_bytes += f.size;
                    } else {
                        copy_n += 1;
                        copy_bytes += f.size;
                    }
                    if let Some(p) = f.abs_path().parent() {
                        touched_dirs.insert(p.to_path_buf());
                    }
                }
                Err(e) => {
                    if rep.errors.len() < 200 {
                        rep.errors.push(format!("{}: {e}", it.src));
                    }
                }
            }
        }
        rep.versions = ver_n;
        for (b, n, bytes, name) in [(&copies_batch, copy_n, copy_bytes, "복사본·사본 정리"), (&versions_batch, ver_n, ver_bytes, "축소본 격리 (확인 후 비우기)")] {
            journal::finish_batch(&conn, b)?;
            if n == 0 {
                conn.execute("DELETE FROM batches WHERE id = ?", [b])?;
            } else {
                conn.execute(
                    "UPDATE batches SET description = ? WHERE id = ?",
                    rusqlite::params![format!("{prefix}{name} {n}개 ({})", human_bytes(bytes)), b],
                )?;
            }
        }
        if copy_n > 0 {
            rep.copies_batch = copies_batch.clone();
        }
        if ver_n > 0 {
            rep.versions_batch = versions_batch;
        }
        // Deleted for good, each after a byte-for-byte comparison with the file kept instead.
        if xo.purge && copy_n > 0 && !rep.cancelled {
            let pr = journal::purge_batch_with(&conn, &copies_batch, stop, &|p: Progress| {
                progress(Progress::new("organize:run", "verify", p.done, p.total, p.message.clone()))
            })?;
            rep.purged = pr.deleted;
            rep.purged_bytes = pr.bytes;
            rep.purge_kept = pr.kept + pr.failed;
            rep.cancelled |= pr.cancelled;
            for e in pr.errors {
                if rep.errors.len() < 200 {
                    rep.errors.push(e);
                }
            }
        }
    }

    // 2. Photos the user put on the delete list.
    // A review never empties a group: a decision that keeps none of its group's photos (a group
    // that changed since it was reviewed) deletes nothing from that group.
    let mut drop_ids: Vec<i64> = xo.drop_ids.clone();
    let mut decisions = xo.decisions.clone();
    {
        let mut spared: HashSet<i64> = HashSet::new();
        decisions.retain(|d| {
            let members = crate::triage::members(&conn, &d.key).unwrap_or_default();
            // Still a group with one of the kept photos in it — or, if the group is gone, at
            // least one of the kept photos still exists.
            let kept_alive = d.keep.iter().any(|k| conn.query_row("SELECT 1 FROM files WHERE id = ? AND missing = 0 AND quarantined = 0", [k], |_| Ok(())).is_ok());
            let ok = if members.is_empty() { kept_alive } else { members.iter().any(|m| d.keep.contains(m)) };
            if !ok {
                spared.extend(members);
                spared.extend(d.drop.iter().copied());
            }
            ok
        });
        let before = drop_ids.len();
        drop_ids.retain(|id| !spared.contains(id));
        if drop_ids.len() < before && rep.errors.len() < 200 {
            rep.errors.push(format!("검토한 뒤 바뀐 묶음의 사진 {}장은 지우지 않았습니다 — 다시 검토하세요", before - drop_ids.len()));
            rep.drop_kept += (before - drop_ids.len()) as u64;
        }
    }
    if !drop_ids.is_empty() && !rep.cancelled {
        // Photos on other drives are never deleted: they are just not brought in (and remembered).
        let elsewhere: HashSet<i64> = plan.items.iter().filter(|i| i.cross_volume).map(|i| i.file_id).collect();
        let (keep_out, del): (Vec<i64>, Vec<i64>) = drop_ids.iter().copied().partition(|id| elsewhere.contains(id));
        rep.not_brought = keep_out.len() as u64;
        let d = delete_for_good(db, &del, &keep_out, true, &format!("{prefix}골라서 지운 사진 {}장", del.len()), stop, progress)?;
        let keep_out: HashSet<i64> = keep_out.into_iter().collect();
        work.retain(|i| !keep_out.contains(&i.file_id));
        rep.dropped = d.deleted;
        rep.dropped_bytes = d.bytes;
        rep.drop_kept += d.kept_protected + d.failed;
        rep.drop_batch = d.batch_id;
        for e in d.errors {
            if rep.errors.len() < 200 {
                rep.errors.push(e);
            }
        }
        // Gone now: nothing to move for them.
        let gone: HashSet<i64> =
            del.iter().copied().filter(|id| conn.query_row("SELECT 1 FROM files WHERE id = ? AND quarantined = 0", [*id], |_| Ok(())).is_err()).collect();
        work.retain(|i| !gone.contains(&i.file_id));
    }
    if !decisions.is_empty() {
        let mut c = db.conn()?;
        crate::triage::commit(&mut c, &decisions)?;
    }

    let batch = journal::new_batch(&conn, "organize", &what)?;
    rep.batch_id = batch.clone();
    // Folder listings for finding sidecars (one read per source folder).
    let mut listings: HashMap<PathBuf, Vec<String>> = HashMap::new();
    let total = work.len() as u64;
    // Room for copies from other drives: measured once they start (after the copies above were
    // deleted), always leaving a reserve free.
    let mut room: Option<i64> = None;
    let mut leftover = 0u64;
    let mut counted: HashSet<PathBuf> = HashSet::new();

    // Index updates are committed every few hundred files instead of one by one (much faster on
    // big libraries). A crash loses at most that many index updates; the next scan recognises
    // those files at their new place by content, so nothing is lost or read again.
    conn.execute_batch("BEGIN")?;
    for (i, it) in work.iter().enumerate() {
        if stop.is_stopped() {
            rep.cancelled = true;
            break;
        }
        if i % 10 == 0 {
            progress(Progress::new("organize:run", "run", i as u64, total, it.src.clone()));
        }
        if i > 0 && i % 200 == 0 {
            conn.execute_batch("COMMIT; BEGIN")?;
        }
        if it.action == Action::Copy {
            let left = room.get_or_insert_with(|| match volumes::disk_space(&target) {
                Some((free, total)) => free as i64 - volumes::space_reserve(total) as i64,
                None => i64::MAX,
            });
            if it.size > *left {
                rep.space_skipped += 1;
                rep.space_skipped_bytes += it.size;
                continue;
            }
            *left -= it.size;
        }
        let res: Result<()> = (|| {
            let Some(f) = db::get_file(&conn, it.file_id)? else { bail!("인덱스에서 사라짐") };
            let src = f.abs_path();
            if src.to_string_lossy() != it.src {
                bail!("계획 이후 파일 위치가 바뀌었습니다: {}", it.src);
            }
            if !src.is_file() {
                bail!("파일이 없습니다: {}", it.src);
            }
            let dst = PathBuf::from(&it.dst);
            let dst_rel = volumes::rel_of(&tvol.root, &dst)?;
            let copy = it.action == Action::Copy;
            // The folder's names, read before anything in it moves (so a RAW beside the JPG is seen).
            // The event folder too (its photos may all be in subfolders): counted, not used.
            if !copy && it.event.is_some() && !it.from.is_empty() {
                let ev = volumes::join_rel(Path::new(&f.mount), &it.from);
                if Some(ev.as_path()) != src.parent() && counted.insert(ev.clone()) {
                    leftover += std::fs::read_dir(&ev)
                        .map(|rd| {
                            rd.flatten()
                                .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                                .filter(|e| {
                                    let n = e.file_name().to_string_lossy().to_lowercase();
                                    crate::media::kind_for_ext(&crate::media::ext_of(&n)).is_none() && !is_sidecar_name(&n)
                                })
                                .count() as u64
                        })
                        .unwrap_or(0);
                }
            }
            if let Some(dir) = src.parent() {
                if !listings.contains_key(dir) {
                    let names: Vec<String> =
                        std::fs::read_dir(dir).map(|rd| rd.flatten().filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false)).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
                    // Files that are not photos / videos (nor sidecars) stay behind: say how many.
                    if !copy && counted.insert(dir.to_path_buf()) {
                        leftover += names
                            .iter()
                            .filter(|n| {
                                let n = n.to_lowercase();
                                crate::media::kind_for_ext(&crate::media::ext_of(&n)).is_none() && !is_sidecar_name(&n)
                            })
                            .count() as u64;
                    }
                    listings.insert(dir.to_path_buf(), names);
                }
            }
            if copy {
                journal::copy_indexed(&conn, &batch, &f, &dst, tvol_id, &dst_rel)?;
            } else {
                journal::move_indexed(&conn, &batch, &f, &dst, tvol_id, &dst_rel)?;
                if let Some(p) = src.parent() {
                    touched_dirs.insert(p.to_path_buf());
                }
            }
            // .xmp / .aae / .thm / Takeout .json beside the photo travel with it.
            if let Some(names) = src.parent().and_then(|d| listings.get_mut(d)) {
                for (side, side_dst) in sidecars(&src, &dst, names) {
                    match journal::move_sidecar(&conn, &batch, f.id, &side, &side_dst, copy) {
                        Ok(()) => {
                            rep.sidecars += 1;
                            if !copy {
                                let gone = side.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                                names.retain(|n| *n != gone);
                            }
                        }
                        Err(e) => {
                            if rep.errors.len() < 200 {
                                rep.errors.push(format!("{}: {e}", side.display()));
                            }
                        }
                    }
                }
            }
            Ok(())
        })();
        match res {
            Ok(()) => {
                rep.done += 1;
                rep.bytes += it.size;
            }
            Err(e) => {
                rep.failed += 1;
                if rep.errors.len() < 200 {
                    rep.errors.push(format!("{}: {e}", it.src));
                }
            }
        }
    }
    conn.execute_batch("COMMIT")?;
    if plan.options.remove_empty_dirs {
        for d in touched_dirs {
            // Never remove a source root itself or anything above it.
            if let Some(root) = source_roots.iter().filter(|r| d.starts_with(r)).max_by_key(|r| r.as_os_str().len()) {
                journal::prune_empty_dirs(&d, root);
            }
        }
    }
    rep.leftover_files = leftover;
    journal::finish_batch(&conn, &batch)?;
    if work.is_empty() {
        conn.execute("DELETE FROM batches WHERE id = ? AND op_count = 0", [&batch])?;
    }
    progress(Progress::new("organize:run", "done", total, total, ""));
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, taken: &str, kind: &str, camera: Option<&str>) -> FileRow {
        let dt = chrono::NaiveDateTime::parse_from_str(taken, "%Y-%m-%d %H:%M:%S").unwrap();
        FileRow {
            id: 1,
            volume_id: 1,
            rel_path: format!("x/{name}"),
            name: name.into(),
            ext: crate::media::ext_of(name),
            kind: kind.into(),
            size: 10,
            mtime: 0,
            taken_at: Some(meta::to_ts(dt)),
            date_source: "exif".into(),
            width: None,
            height: None,
            duration_ms: None,
            camera: camera.map(String::from),
            lat: None,
            lon: None,
            orientation: None,
            hash: None,
            missing: false,
            quarantined: false,
            mount: "/".into(),
            online: true,
            clone_of: None,
        }
    }

    #[test]
    fn templates() {
        let f = row("IMG_1.jpg", "2019-05-12 14:30:12", "photo", Some("Apple iPhone 13"));
        let l = vec![];
        let ctx = TemplateCtx { file: &f, labels: &l, trip: None, person: None };
        assert_eq!(render_template("{year}/{year}-{month}", &ctx), "2019/2019-05");
        assert_eq!(render_template("{kind}/{year}/{camera}", &ctx), "사진/2019/Apple iPhone 13");
        assert_eq!(render_template("{source}/{date}", &ctx), "카메라/2019-05-12");
        let l = vec!["screenshot".to_string(), "src:kakaotalk".to_string()];
        let ctx = TemplateCtx { file: &f, labels: &l, trip: None, person: None };
        assert_eq!(render_template("{kind}/{source}", &ctx), "스크린샷/카카오톡");
        // {trip}: trip folder for trip photos, nothing (= parent folder) for the rest.
        assert_eq!(render_template("{year}/{trip}", &ctx), "2019");
        let ctx = TemplateCtx { file: &f, labels: &l, trip: Some("2019년 5월 Honolulu 여행"), person: None };
        assert_eq!(render_template("{year}/{trip}", &ctx), "2019/2019년 5월 Honolulu 여행");
        let ctx = TemplateCtx { file: &f, labels: &l, trip: None, person: Some("아빠, 엄마") };
        assert_eq!(render_template("{person}/{year}", &ctx), "아빠, 엄마/2019");
        let ctx = TemplateCtx { file: &f, labels: &l, trip: None, person: None };
        assert_eq!(render_template("{person}/{year}", &ctx), "2019", "nobody named: stays at the year level");
        assert_eq!(target_name(&f, Rename::DatePrefix), "2019-05-12_143012_IMG_1.jpg");
    }

    #[test]
    fn sanitizing() {
        assert_eq!(sanitize("a:b?c"), "a_b_c");
        assert_eq!(sanitize("CON"), "_CON");
        assert_eq!(sanitize("trip. "), "trip");
        assert_eq!(with_suffix("a.jpg", 2), "a_2.jpg");
    }
}

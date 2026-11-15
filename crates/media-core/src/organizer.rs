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
}

#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub id: String,
    pub options: OrganizeOptions,
    pub items: Vec<PlanItem>,
    pub summary: PlanSummary,
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
    let placements = if library {
        // Same-size files get their 64 KiB fingerprint (once), so copies are recognised.
        crate::identity::backfill_prehashes(&conn)?;
        let lib_vol: i64 = conn.query_row("SELECT id FROM volumes WHERE uid = ?", [&tvol.uid], |r| r.get(0)).unwrap_or(-1);
        crate::library::layout(&conn, lib_vol, &ids, &opts.library, &crate::library::load_rules(&conn)?)?
    } else {
        HashMap::new()
    };
    let mut events: HashSet<String> = HashSet::new();
    let mut from_dirs: HashSet<PathBuf> = HashSet::new();
    let mut items = Vec::with_capacity(ids.len());
    let mut summary = PlanSummary::default();
    // dst key -> (size, src path, known hash) of the file that claimed it first in this plan.
    let mut claimed: HashMap<String, (i64, PathBuf, Option<String>)> = HashMap::new();
    let mut folders: HashSet<String> = HashSet::new();
    // What already sits in each destination folder (lower-cased name -> size), read once per
    // folder instead of a disk lookup per file.
    let mut on_disk: HashMap<String, HashMap<String, i64>> = HashMap::new();
    let total = ids.len() as u64;
    let empty = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        if stop.is_stopped() {
            bail!("cancelled");
        }
        if i % 500 == 0 {
            progress(Progress::new("organize:plan", "plan", i as u64, total, ""));
        }
        let Some(f) = db::get_file(&conn, *id)? else { continue };
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
                let remove = p.act == crate::library::Act::Remove;
                if remove {
                    summary.remove += 1;
                    summary.remove_bytes += f.size;
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
                    tag: p.tag.into(),
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
            cross_volume: f.volume_id_uid_differs(&conn, &tvol.uid),
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
            if let Some((csize, csrc, chash)) = claimed.get(&k) {
                // Two identical files from different drives: keep one, skip the other.
                if *csize == f.size && same_bytes(chash.as_deref(), csrc, f.hash.as_deref(), &src) {
                    item.action = Action::SkipDuplicate;
                    break cand;
                }
                n += 1;
                continue;
            }
            let listing = on_disk.entry(key(&dir)).or_insert_with(|| list_dir(&dir));
            if let Some(size) = listing.get(&name.to_lowercase()).copied() {
                // Same bytes already there? Then this file is a duplicate: skip it.
                let same = size == f.size && same_bytes(None, &cand, f.hash.as_deref(), &src);
                if same {
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
            claimed.insert(key(&dst), (f.size, src.clone(), f.hash.clone()));
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
                "raw" | "live" | "edited" => summary.paired += 1,
                _ => {}
            }
            if p.rel_dir.starts_with("큰 동영상/") {
                summary.big_videos += 1;
            }
            if let Some(e) = &p.event {
                events.insert(format!("{}|{e}", p.rel_dir));
            }
            if item.action == Action::Move {
                if let Some(parent) = src.parent() {
                    from_dirs.insert(parent.to_path_buf());
                }
                // The event folder itself too (its photos may all be in subfolders).
                if p.event.is_some() && !p.from_dir.is_empty() {
                    from_dirs.insert(volumes::join_rel(Path::new(&f.mount), &p.from_dir));
                }
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
    // Files that are not photos / videos (nor their sidecars) stay behind in the folders photos
    // move out of: count them so the preview can say so.
    for d in &from_dirs {
        summary.leftover_files += std::fs::read_dir(d)
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
    progress(Progress::new("organize:plan", "done", total, total, ""));
    Ok(Plan { id: uuid::Uuid::new_v4().simple().to_string(), options: opts.clone(), items, summary })
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
        let follower = |t: &str| matches!(t, "raw" | "live" | "edited" | "");
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
    let mut summary = PlanSummary::default();
    for it in &items {
        match it.action {
            Action::Move => summary.moves += 1,
            Action::Copy => {
                summary.copies += 1;
                summary.copy_bytes += it.size;
            }
            Action::Remove => summary.remove += 1,
            _ => {}
        }
    }
    Plan { id: plan.id.clone(), options: plan.options.clone(), items, summary }
}

impl FileRow {
    fn volume_id_uid_differs(&self, conn: &rusqlite::Connection, uid: &str) -> bool {
        conn.query_row("SELECT uid FROM volumes WHERE id = ?", [self.volume_id], |r| r.get::<_, String>(0))
            .map(|u| u != uid)
            .unwrap_or(true)
    }
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
    /// Library: copies on the library drive quarantined, and ones kept (twin not in place, or protected).
    pub removed: u64,
    pub removed_bytes: i64,
    pub remove_skipped: u64,
    pub errors: Vec<String>,
    pub cancelled: bool,
}

pub fn execute(db: &Db, plan: &Plan, stop: &StopFlag, progress: ProgressFn) -> Result<ExecReport> {
    let conn = db.conn()?;
    volumes::refresh(&conn)?;
    std::fs::create_dir_all(plan.options.target_dir.trim())?;
    let target = volumes::canonical(Path::new(plan.options.target_dir.trim()))?;
    // The library folder becomes a source, so it stays indexed from now on.
    volumes::add_source(&conn, &target)?;
    let tvol = volumes::identify(&target)?;
    let tvol_id = volumes::upsert(&conn, &tvol)?;

    let work: Vec<&PlanItem> = plan.items.iter().filter(|i| matches!(i.action, Action::Move | Action::Copy)).collect();
    // Copies from other drives need room on the library drive (with a little margin).
    if plan.summary.copy_bytes > 0 {
        if let Some(free) = volumes::free_space(&target) {
            let need = plan.summary.copy_bytes as u64 + 512 * 1024 * 1024;
            if free < need {
                bail!(
                    "공간이 부족합니다: 복사할 {:.1} GB, 남은 공간 {:.1} GB",
                    plan.summary.copy_bytes as f64 / 1e9,
                    free as f64 / 1e9
                );
            }
        }
    }
    let library = plan.options.layout == "library";
    let what = if library {
        let n = |a: Action| plan.items.iter().filter(|i| i.action == a).count();
        let parts: Vec<String> = [("이동", n(Action::Move)), ("복사", n(Action::Copy)), ("사본 정리", n(Action::Remove))]
            .iter()
            .filter(|(_, c)| *c > 0)
            .map(|(w, c)| format!("{w} {c}"))
            .collect();
        format!("라이브러리로 모으기 → {} ({})", target.display(), parts.join(" · "))
    } else {
        let verb = if plan.options.mode == Mode::Move { "이동" } else { "복사" };
        format!("{} → {} ({}개 {verb})", plan.options.template, target.display(), work.len())
    };
    let batch = journal::new_batch(&conn, "organize", &what)?;
    let mut rep = ExecReport { batch_id: batch.clone(), ..Default::default() };
    // Folder listings for finding sidecars (one read per source folder).
    let mut listings: HashMap<PathBuf, Vec<String>> = HashMap::new();
    let mut touched_dirs: HashSet<PathBuf> = HashSet::new();
    let source_roots: Vec<PathBuf> = db::list_sources(&conn)?.into_iter().map(|s| PathBuf::from(s.abs_path)).collect();
    let total = work.len() as u64;

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
            if let Some(dir) = src.parent() {
                listings.entry(dir.to_path_buf()).or_insert_with(|| {
                    std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default()
                });
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
    // Copies on the library drive: removed (quarantined, in this same undoable batch) once the
    // kept twin is really in place. Protected files (♥, original, keep folders) stay.
    let removes: Vec<&PlanItem> = plan.items.iter().filter(|i| i.action == Action::Remove).collect();
    if !removes.is_empty() && !rep.cancelled {
        let ids: Vec<i64> = removes.iter().map(|i| i.file_id).collect();
        let protected: HashSet<i64> = crate::protect::check(&conn, &ids)?.into_iter().map(|b| b.id).collect();
        for it in removes {
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
            match journal::quarantine(&conn, &batch, &f) {
                Ok(()) => {
                    rep.removed += 1;
                    rep.removed_bytes += f.size;
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
    }
    if plan.options.remove_empty_dirs {
        for d in touched_dirs {
            // Never remove a source root itself or anything above it.
            if let Some(root) = source_roots.iter().filter(|r| d.starts_with(r)).max_by_key(|r| r.as_os_str().len()) {
                journal::prune_empty_dirs(&d, root);
            }
        }
    }
    journal::finish_batch(&conn, &batch)?;
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

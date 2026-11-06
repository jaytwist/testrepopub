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
    let mut items = Vec::with_capacity(ids.len());
    let mut summary = PlanSummary::default();
    // dst key -> (size, src path, known hash) of the file that claimed it first in this plan.
    let mut claimed: HashMap<String, (i64, PathBuf, Option<String>)> = HashMap::new();
    let mut folders: HashSet<String> = HashSet::new();
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
        if uncertain {
            summary.uncertain_dates += 1;
        }
        let ctx = TemplateCtx { file: &f, labels, trip: trips.get(id).map(String::as_str), person: persons.get(id).map(String::as_str) };
        let rel_dir = match (&opts.uncertain_folder, uncertain) {
            (Some(u), true) if !u.trim().is_empty() => format!("{}/{}", sanitize(u.trim()), render_template(&opts.template, &ctx)),
            _ => render_template(&opts.template, &ctx),
        };
        let dir = volumes::join_rel(&target, &rel_dir);
        let base_name = target_name(&f, opts.rename);
        let mut item = PlanItem {
            file_id: f.id,
            src: src.to_string_lossy().into_owned(),
            dst: String::new(),
            action: if opts.mode == Mode::Move { Action::Move } else { Action::Copy },
            renamed: false,
            cross_volume: f.volume_id_uid_differs(&conn, &tvol.uid),
            size: f.size,
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
            if cand.exists() {
                // Same bytes already there? Then this file is a duplicate: skip it.
                let same = std::fs::metadata(&cand).map(|m| m.len() as i64 == f.size).unwrap_or(false)
                    && same_bytes(None, &cand, f.hash.as_deref(), &src);
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
            Action::Copy => summary.copies += 1,
            Action::InPlace => summary.in_place += 1,
            Action::SkipDuplicate => summary.skip_duplicate += 1,
            Action::SkipOffline => summary.skip_offline += 1,
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
    progress(Progress::new("organize:plan", "done", total, total, ""));
    Ok(Plan { id: uuid::Uuid::new_v4().simple().to_string(), options: opts.clone(), items, summary })
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
    let verb = if plan.options.mode == Mode::Move { "이동" } else { "복사" };
    let batch = journal::new_batch(&conn, "organize", &format!("{} → {} ({}개 {verb})", plan.options.template, target.display(), work.len()))?;
    let mut rep = ExecReport { batch_id: batch.clone(), ..Default::default() };
    let mut touched_dirs: HashSet<PathBuf> = HashSet::new();
    let source_roots: Vec<PathBuf> = db::list_sources(&conn)?.into_iter().map(|s| PathBuf::from(s.abs_path)).collect();
    let total = work.len() as u64;

    for (i, it) in work.iter().enumerate() {
        if stop.is_stopped() {
            rep.cancelled = true;
            break;
        }
        if i % 10 == 0 {
            progress(Progress::new("organize:run", "run", i as u64, total, it.src.clone()));
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
            match plan.options.mode {
                Mode::Move => {
                    journal::move_indexed(&conn, &batch, &f, &dst, tvol_id, &dst_rel)?;
                    if let Some(p) = src.parent() {
                        touched_dirs.insert(p.to_path_buf());
                    }
                }
                Mode::Copy => {
                    journal::copy_indexed(&conn, &batch, &f, &dst, tvol_id, &dst_rel)?;
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

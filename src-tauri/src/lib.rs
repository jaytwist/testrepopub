//! Tauri shell: a thin command layer over `media-core`.
//!
//! * Long jobs (scan, thumbnails, dedupe, plan, organize, undo) run one at a time on a worker
//!   thread and report through `job-progress` / `job-done` events.
//! * Thumbnails are served through the `thumb://` protocol, generated on demand and cached.
//! * Full-size previews use Tauri's asset protocol (`convertFileSrc`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use media_core::db::{self, FileFilter, FileRow, FilterOptions, FilteredStats, GridItem, GroupRow, LibraryStats, MonthCount, SourceRow, VolumeRow};
use media_core::dedupe::{self, DupGroup, DupKind, DupSummary, Similarity};
use media_core::journal::{self, BatchRow};
use media_core::labels::{self, LabelCount, LabelRow};
use media_core::organizer::{self, OrganizeOptions, Plan, PlanItem, PlanSummary};
use media_core::scanner;
use media_core::userfiles::{self, ScanOptions};
use media_core::thumbs::{self, ThumbCache};
use media_core::{meta, volumes, Db, Locked, Progress, StopFlag};
use serde::Serialize;
use tauri::http::{Response, StatusCode};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;

mod lane;
mod logs;

use lane::{Lane, LaneStatus};
use media_core::analysis::{self, AnalysisOptions};

type CmdResult<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    format!("{e:#}")
}

struct AppState {
    db: Db,
    thumbs: ThumbCache,
    data_dir: PathBuf,
    cache_dir: PathBuf,
    stop: StopFlag,
    job: Mutex<Option<String>>,
    plans: Mutex<HashMap<String, Arc<Plan>>>,
    lane: Arc<Lane>,
    /// Work a crash or power loss cut short last time (told to the user once, then files are
    /// checked again).
    interrupted: Mutex<Vec<String>>,
    /// Thumbnails: a quick lane (look up, serve what is cached) and a lane that makes the
    /// missing ones — a few at a time, the newest request first.
    thumb_fast: media_core::WorkStack,
    thumb_make: media_core::WorkStack,
    /// Raised whenever files or what is known about them changed: lists computed from the
    /// index are reused until then.
    data_gen: std::sync::atomic::AtomicU64,
    folders_cache: media_core::folders::FoundCache,
    /// The comparison asked for last by the side-by-side window: an older one gives up.
    walk_ticket: std::sync::atomic::AtomicU64,
    /// Held while a setting is read, changed and written back.
    settings_lock: Mutex<()>,
    /// The window was closed while a job ran: the job is stopping, and the app exits once it
    /// has (never in the middle of moving or deleting a file).
    closing: std::sync::atomic::AtomicBool,
}

impl AppState {
    fn changed(&self) {
        self.data_gen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Recompute data derived from the per-file analysis (bursts, places, trips, people).
/// Each step runs even if another failed (a cleanup error once kept people from ever being grouped).
pub(crate) fn post_analysis(st: &AppState) -> anyhow::Result<()> {
    st.changed();
    let mut conn = st.db.conn()?;
    let mut errors = Vec::new();
    if let Err(e) = media_core::cleanup::refresh(&mut conn) {
        errors.push(format!("정리 제안 계산 오류: {e:#}"));
    }
    // Similar photos from the stored fingerprints (no files read; under a second for a big
    // library), so each year's 「비슷한 사진」 review is ready without a manual search. Decisions
    // already made are kept.
    if let Err(e) = media_core::dedupe::find_similar_images(&st.db, media_core::dedupe::Similarity::High, &StopFlag::new(), &|_| {}) {
        errors.push(format!("비슷한 사진 찾기 오류: {e:#}"));
    }
    if let Err(e) = media_core::people::refresh(&mut conn) {
        errors.push(format!("인물 묶기 오류: {e:#}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(errors.join(" · "))
    }
}

fn upgrade_dup_keepers(st: &AppState) -> anyhow::Result<()> {
    let mut conn = st.db.conn()?;
    let done: Option<String> =
        media_core::rusqlite::OptionalExtension::optional(conn.query_row("SELECT value FROM settings WHERE key = 'dup_decide_v1'", [], |r| r.get(0)))?;
    if done.is_none() {
        media_core::dedupe::apply_rules(&mut conn)?;
        conn.execute("INSERT OR REPLACE INTO settings(key, value) VALUES ('dup_decide_v1', '1')", [])?;
    }
    Ok(())
}

pub(crate) fn refresh_places(st: &AppState) -> anyhow::Result<()> {
    let mut conn = st.db.conn()?;
    media_core::places::refresh(&mut conn)?;
    // Which copy of identical files is the original (cheap; nothing is read from disk).
    media_core::identity::refresh_roles(&mut conn)?;
    Ok(())
}

#[derive(Clone, Serialize)]
struct JobDone {
    job: String,
    ok: bool,
    error: Option<String>,
    result: serde_json::Value,
}

/// Jobs that move or delete files (by the start of their name).
const DESTRUCTIVE: &[&str] = &["organize:run", "cleanup:", "dedupe:apply", "delete", "purge", "undo", "library:", "import"];

/// Save a copy of the database unless one was saved within `every`. Failures are logged, never
/// in the way of the work.
fn snapshot_if_due(st: &AppState, why: &str, every: std::time::Duration) {
    let fresh = st.db.snapshots().first().and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|age| age < every);
    if fresh {
        return;
    }
    let t = std::time::Instant::now();
    match st.db.snapshot(&why.replace(':', "-"), 3) {
        Ok(p) => log::info!(target: "app", "database copy saved in {:.1?}: {}", t.elapsed(), p.display()),
        Err(e) => log::warn!(target: "app", "database copy failed: {e:#}"),
    }
}

/// For commands that change what a running job works from (keep rules, sources, people data):
/// refused while a job runs, with a reason, instead of changing things under it.
fn no_job(state: &AppState) -> CmdResult<()> {
    match state.job.locked().as_ref() {
        Some(j) => Err(format!("작업이 진행 중입니다({j}). 끝난 뒤에 다시 해 주세요.")),
        None => Ok(()),
    }
}

/// Run `f` on a worker thread as the single active job.
fn spawn_job<F, R>(app: &AppHandle, state: &AppState, name: &str, f: F) -> CmdResult<()>
where
    F: FnOnce(&AppState, &(dyn Fn(Progress) + Sync)) -> anyhow::Result<R> + Send + 'static,
    R: Serialize,
{
    {
        let mut j = state.job.locked();
        if let Some(cur) = j.as_ref() {
            return Err(format!("다른 작업이 진행 중입니다: {cur}"));
        }
        *j = Some(name.to_string());
    }
    state.stop.reset();
    // Background analysis yields to anything the user starts.
    state.lane.set_job_active(app, true);
    let app = app.clone();
    let name = name.to_string();
    std::thread::spawn(move || {
        let st = app.state::<AppState>();
        let emit_app = app.clone();
        let last = Mutex::new(std::time::Instant::now() - std::time::Duration::from_secs(1));
        let progress = move |p: Progress| {
            // Throttle UI updates to ~15/s, but always deliver stage changes and completion.
            let mut l = last.locked();
            if l.elapsed().as_millis() >= 66 || p.done == p.total || p.done == 0 {
                *l = std::time::Instant::now();
                let _ = emit_app.emit("job-progress", p);
            }
        };
        // Debug builds only: a job that takes a while on demand (`MO_SLOW_JOB_MS`), to try what
        // happens when the window is closed meanwhile.
        #[cfg(debug_assertions)]
        if let Some(ms) = std::env::var("MO_SLOW_JOB_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        log::info!(target: "app", "job {name}: start");
        let started = std::time::Instant::now();
        // Before work that moves or deletes files: a copy of the database, if the last one is
        // not from today's session (it is the only record of where quarantined files belong).
        if DESTRUCTIVE.iter().any(|d| name.starts_with(d)) {
            snapshot_if_due(&st, &name, std::time::Duration::from_secs(6 * 3600));
        }
        // A panic inside a job must not leave the app saying "작업 중" for ever.
        let res = media_core::guarded("작업", || f(&st, &progress));
        match &res {
            Ok(_) => log::info!(target: "app", "job {name}: done in {:.1?}", started.elapsed()),
            Err(e) => log::warn!(target: "app", "job {name}: failed after {:.1?}: {e:#}", started.elapsed()),
        }
        // After work that wrote a lot: refresh planner statistics and fold the write-ahead log
        // back (both cheap; failures do not matter).
        if let Ok(conn) = st.db.conn() {
            let _ = conn.execute_batch("PRAGMA optimize; PRAGMA wal_checkpoint(PASSIVE);");
        }
        st.changed();
        // New GPS files, deletions, moves: keep places and trips current (cheap). (Guarded: the
        // end of the job is announced whatever happens here.)
        if let Err(e) = media_core::guarded("장소 정리", || refresh_places(&st)) {
            log::warn!(target: "app", "places refresh failed: {e:#}");
        }
        // The job is over only now, right before saying so: a job started in between would
        // otherwise be told "done" by this one's ending.
        *st.job.locked() = None;
        st.lane.set_job_active(&app, false);
        if name == "scan" {
            st.lane.kick(&app, false);
        }
        // Fingerprints for entries indexed by older versions (needed to recognise copies): read
        // behind, whether or not automatic analysis is on.
        if name == "scan" || name == "organize:plan" {
            st.lane.kick_backfill(&app);
        }
        let done = match res {
            Ok(r) => JobDone { job: name, ok: true, error: None, result: serde_json::to_value(r).unwrap_or_default() },
            Err(e) => JobDone { job: name, ok: false, error: Some(format!("{e:#}")), result: serde_json::Value::Null },
        };
        let _ = app.emit("job-done", done);
        // The window was closed meanwhile: the job has stopped at a safe point, now the app goes.
        if st.closing.load(std::sync::atomic::Ordering::SeqCst) {
            app.exit(0);
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// App / sources
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
struct AppInfo {
    version: String,
    data_dir: String,
    cache_dir: String,
    ffmpeg: bool,
    job: Option<String>,
    /// Work cut short last time (given once).
    interrupted: Vec<String>,
    /// Things the user should know before working (two drives under one identity, …).
    warnings: Vec<String>,
}

#[tauri::command(async)]
fn app_info(state: State<AppState>) -> AppInfo {
    let warnings = state
        .db
        .conn()
        .ok()
        .and_then(|c| volumes::ambiguous(&c).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|(label, at)| format!("‘{label}’ 드라이브가 같은 일련번호로 두 곳({})에 연결되어 있습니다. 복제한 디스크는 구분할 수 없어 둘 다 연결되지 않은 것으로 다룹니다 — 하나를 뽑아 주세요.", at.join(", ")))
        .collect();
    AppInfo {
        warnings,
        version: env!("CARGO_PKG_VERSION").into(),
        data_dir: state.data_dir.to_string_lossy().into(),
        cache_dir: state.cache_dir.to_string_lossy().into(),
        ffmpeg: dedupe::ffmpeg_available(),
        job: state.job.locked().clone(),
        interrupted: std::mem::take(&mut *state.interrupted.locked()),
    }
}

#[tauri::command(async)]
fn list_volumes(state: State<AppState>, refresh: bool) -> CmdResult<Vec<VolumeRow>> {
    let conn = state.db.conn().map_err(err)?;
    if refresh {
        volumes::refresh(&conn).map_err(err)?;
    }
    db::list_volumes(&conn).map_err(err)
}

#[tauri::command(async)]
fn list_sources(state: State<AppState>) -> CmdResult<Vec<SourceRow>> {
    let conn = state.db.conn().map_err(err)?;
    db::list_sources(&conn).map_err(err)
}

#[tauri::command(async)]
fn add_source(state: State<AppState>, path: String) -> CmdResult<i64> {
    let conn = state.db.conn().map_err(err)?;
    let id = volumes::add_source(&conn, Path::new(&path)).map_err(err)?;
    state.changed();
    Ok(id)
}

/// Stop tracking a folder. Index rows for files under it are removed; files on disk are untouched.
#[tauri::command(async)]
fn remove_source(state: State<AppState>, id: i64) -> CmdResult<()> {
    no_job(&state)?;
    let conn = state.db.conn().map_err(err)?;
    volumes::remove_source(&conn, id).map_err(err)?;
    state.changed();
    Ok(())
}

#[tauri::command(async)]
/// Check for new files: one source, one drive (`volume_id`), one folder (`folder`, inside a
/// registered source — nothing else is listed), or everything.
fn start_scan(
    app: AppHandle,
    state: State<AppState>,
    source_id: Option<i64>,
    volume_id: Option<i64>,
    folder: Option<String>,
    full: Option<bool>,
) -> CmdResult<()> {
    // Quick check by default (unchanged folders are not listed); `full` lists everything.
    let mode = if full.unwrap_or(false) { scanner::Mode::Full } else { scanner::Mode::Auto };
    // Resolve the folder first so a wrong pick is reported right away.
    let scope = match &folder {
        Some(f) => {
            let conn = state.db.conn().map_err(err)?;
            Some(scanner::locate(&conn, std::path::Path::new(f)).map_err(err)?.ok_or("등록된 폴더 안의 폴더를 골라 주세요")?)
        }
        None => None,
    };
    spawn_job(&app, &state, "scan", move |st, progress| {
        let conn = st.db.conn()?;
        let opts = userfiles::load_options(&conn)?;
        let auto_analysis = analysis::load_options(&conn)?.auto;
        drop(conn);
        let stats = match (scope, source_id, volume_id) {
            (Some((id, sub)), _, _) => {
                vec![scanner::scan_with(&st.db, id, Some(&sub), &opts, mode, &st.stop, progress)?]
            }
            (None, Some(id), _) => vec![scanner::scan_with(&st.db, id, None, &opts, mode, &st.stop, progress)?],
            (None, None, Some(v)) => scanner::scan_volume_with(&st.db, v, &opts, mode, &st.stop, progress)?,
            (None, None, None) => scanner::scan_all_with(&st.db, &opts, mode, &st.stop, progress)?,
        };
        // With background analysis on, thumbnails come from its single-read pass right after this
        // job; otherwise make them here so the drive can be unplugged and still browsed.
        let warm = if auto_analysis { thumbs::WarmStats::default() } else { thumbs::warm(&st.db, &st.thumbs, &st.stop, progress)? };
        Ok(serde_json::json!({ "scan": stats, "thumbs": warm }))
    })
}

// ---------------------------------------------------------------------------------------------
// Import from a camera card
// ---------------------------------------------------------------------------------------------

#[tauri::command(async)]
fn list_cards() -> Vec<media_core::importer::Card> {
    media_core::importer::find_cards()
}

#[tauri::command(async)]
fn get_import_settings(state: State<AppState>) -> CmdResult<media_core::importer::ImportSettings> {
    let conn = state.db.conn().map_err(err)?;
    media_core::importer::ImportSettings::load(&conn).map_err(err)
}

#[tauri::command(async)]
fn set_import_settings(state: State<AppState>, settings: media_core::importer::ImportSettings) -> CmdResult<()> {
    let conn = state.db.conn().map_err(err)?;
    state.changed(); // where the library is decides which folder of a set stays
    settings.save(&conn).map_err(err)
}

/// Count what is new on the card (the card is scanned; the backup drive is not).
#[tauri::command(async)]
fn start_import_preview(app: AppHandle, state: State<AppState>, mount: String) -> CmdResult<()> {
    spawn_job(&app, &state, "import:preview", move |st, progress| {
        let opts = userfiles::load_options(&st.db.conn()?)?;
        media_core::importer::preview(&st.db, std::path::Path::new(&mount), &opts, &st.stop, progress)
    })
}

/// Copy the card's new photos into the backup folder (indexed while copying), then forget the card.
#[tauri::command(async)]
fn start_import(app: AppHandle, state: State<AppState>, mount: String) -> CmdResult<()> {
    spawn_job(&app, &state, "import", move |st, progress| {
        let conn = st.db.conn()?;
        let opts = userfiles::load_options(&conn)?;
        let settings = media_core::importer::ImportSettings::load(&conn)?;
        drop(conn);
        media_core::importer::run(&st.db, std::path::Path::new(&mount), &settings, &opts, &st.stop, progress)
    })
}

/// The card was looked at but not imported: forget it again.
#[tauri::command(async)]
fn import_cancel(state: State<AppState>, source_id: i64) -> CmdResult<()> {
    let conn = state.db.conn().map_err(err)?;
    media_core::importer::forget(&conn, source_id).map_err(err)
}

#[tauri::command(async)]
fn get_scan_options(state: State<AppState>) -> CmdResult<ScanOptions> {
    let conn = state.db.conn().map_err(err)?;
    userfiles::load_options(&conn).map_err(err)
}

#[tauri::command(async)]
fn set_scan_options(state: State<AppState>, options: ScanOptions) -> CmdResult<()> {
    let conn = state.db.conn().map_err(err)?;
    userfiles::save_options(&conn, &options).map_err(err)
}

#[tauri::command(async)]
fn default_scan_options() -> ScanOptions {
    ScanOptions::default()
}

#[derive(Serialize)]
struct SuggestedFolder {
    label: String,
    path: String,
    kind: String,
    already_added: bool,
}

/// The user's own media folders (Pictures, Videos, Desktop, Downloads, Documents, OneDrive) and
/// connected external drives: one click to add, instead of scanning all of C:.
#[tauri::command(async)]
fn suggested_folders(app: AppHandle, state: State<AppState>) -> CmdResult<Vec<SuggestedFolder>> {
    let conn = state.db.conn().map_err(err)?;
    let sources: Vec<PathBuf> = db::list_sources(&conn).map_err(err)?.into_iter().map(|s| PathBuf::from(s.abs_path)).collect();
    let covered = |p: &Path| sources.iter().any(|s| p.starts_with(s));
    let paths = app.path();
    let mut cands: Vec<(String, Option<PathBuf>, &str)> = vec![
        ("사진".into(), paths.picture_dir().ok(), "user"),
        ("동영상".into(), paths.video_dir().ok(), "user"),
        ("바탕 화면".into(), paths.desktop_dir().ok(), "user"),
        ("다운로드".into(), paths.download_dir().ok(), "user"),
        ("문서".into(), paths.document_dir().ok(), "user"),
    ];
    for var in ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"] {
        if let Some(p) = std::env::var_os(var) {
            cands.push(("OneDrive".into(), Some(PathBuf::from(p)), "user"));
        }
    }
    let mut out: Vec<SuggestedFolder> = Vec::new();
    for (label, p, kind) in cands {
        let Some(p) = p.filter(|p| p.is_dir()) else { continue };
        if out.iter().any(|o| Path::new(&o.path) == p) {
            continue;
        }
        out.push(SuggestedFolder { label, already_added: covered(&p), path: p.to_string_lossy().into(), kind: kind.into() });
    }
    // External drives: the whole drive is a sensible source (system folders are skipped anyway).
    let mut drives: Vec<_> = volumes::mounted().into_values().filter(|v| v.removable).collect();
    drives.sort_by(|a, b| a.root.cmp(&b.root));
    for v in drives {
        out.push(SuggestedFolder {
            label: format!("외장 드라이브 {} ({})", v.root.display(), v.label),
            already_added: covered(&v.root),
            path: v.root.to_string_lossy().into(),
            kind: "drive".into(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Cleanup suggestions
// ---------------------------------------------------------------------------------------------

// ---------------------------------------------------------------------------------------------
// People
// ---------------------------------------------------------------------------------------------

#[tauri::command(async)]
fn people_list(state: State<AppState>, include_hidden: bool) -> CmdResult<Vec<media_core::people::PersonRow>> {
    let conn = state.db.conn().map_err(err)?;
    media_core::people::list(&conn, include_hidden).map_err(err)
}

#[tauri::command(async)]
fn person_faces(state: State<AppState>, person: i64) -> CmdResult<Vec<media_core::people::FaceRow>> {
    let conn = state.db.conn().map_err(err)?;
    media_core::people::faces_of(&conn, person, 2000).map_err(err)
}

#[tauri::command(async)]
fn people_suggestions(state: State<AppState>) -> CmdResult<Vec<media_core::people::PairSuggestion>> {
    let conn = state.db.conn().map_err(err)?;
    media_core::people::suggestions(&conn, 5).map_err(err)
}

/// Name a person. If someone else already has that name, nothing changes and their id comes
/// back so the UI can offer to merge the two.
#[tauri::command(async)]
fn rename_person(state: State<AppState>, person: i64, name: String) -> CmdResult<Option<i64>> {
    let mut conn = state.db.conn().map_err(err)?;
    if let Some(other) = media_core::people::person_named(&conn, &name, person).map_err(err)? {
        if !name.trim().is_empty() {
            return Ok(Some(other));
        }
    }
    media_core::people::rename(&mut conn, person, &name).map_err(err)?;
    Ok(None)
}

#[tauri::command(async)]
fn merge_people(state: State<AppState>, into: i64, others: Vec<i64>) -> CmdResult<()> {
    let mut conn = state.db.conn().map_err(err)?;
    media_core::people::merge(&mut conn, into, &others).map_err(err)
}

#[tauri::command(async)]
fn move_faces(state: State<AppState>, faces: Vec<i64>, from: i64, to: Option<i64>) -> CmdResult<i64> {
    let mut conn = state.db.conn().map_err(err)?;
    media_core::people::move_faces(&mut conn, &faces, from, to).map_err(err)
}

#[tauri::command(async)]
fn reject_faces(state: State<AppState>, faces: Vec<i64>, person: i64) -> CmdResult<()> {
    let mut conn = state.db.conn().map_err(err)?;
    media_core::people::reject(&mut conn, &faces, person).map_err(err)
}

#[tauri::command(async)]
fn hide_people(state: State<AppState>, people: Vec<i64>, hidden: bool) -> CmdResult<()> {
    let conn = state.db.conn().map_err(err)?;
    media_core::people::set_hidden(&conn, &people, hidden).map_err(err)
}

#[tauri::command(async)]
fn set_person_cover(state: State<AppState>, person: i64, face: i64) -> CmdResult<()> {
    let conn = state.db.conn().map_err(err)?;
    media_core::people::set_cover(&conn, person, face).map_err(err)
}

#[tauri::command(async)]
fn answer_people_pair(state: State<AppState>, a: i64, b: i64, same: bool) -> CmdResult<()> {
    let mut conn = state.db.conn().map_err(err)?;
    media_core::people::answer_pair(&mut conn, a, b, same).map_err(err)
}

/// "Delete face data": turn the feature off and forget every face, person and crop.
#[tauri::command(async)]
fn clear_face_data(state: State<AppState>) -> CmdResult<()> {
    no_job(&state)?;
    let mut conn = state.db.conn().map_err(err)?;
    let mut opts = analysis::load_options(&conn).map_err(err)?;
    opts.people = false;
    analysis::save_options(&conn, &opts).map_err(err)?;
    state.lane.reset_engine();
    media_core::people::clear_all(&mut conn).map_err(err)?;
    let _ = std::fs::remove_dir_all(state.thumbs.dir().join("faces"));
    Ok(())
}

#[tauri::command(async)]
fn trips_summary(state: State<AppState>) -> CmdResult<media_core::places::TripsSummary> {
    let conn = state.db.conn().map_err(err)?;
    media_core::places::summary(&conn).map_err(err)
}

#[tauri::command(async)]
fn cleanup_summary(state: State<AppState>) -> CmdResult<media_core::cleanup::CleanupSummary> {
    let conn = state.db.conn().map_err(err)?;
    media_core::cleanup::summary(&conn).map_err(err)
}

/// Look at analysed files for damage (libraries analysed before the check existed).
#[tauri::command(async)]
fn start_damage_check(app: AppHandle, state: State<AppState>) -> CmdResult<()> {
    spawn_job(&app, &state, "check:damage", move |st, progress| media_core::damage::run(&st.db, &st.stop, progress))
}

#[tauri::command(async)]
fn burst_extra_ids(state: State<AppState>) -> CmdResult<Vec<i64>> {
    let conn = state.db.conn().map_err(err)?;
    media_core::cleanup::burst_extra_ids(&conn).map_err(err)
}

/// "Keep": never suggest these files for `kind` again.
#[tauri::command(async)]
fn dismiss(state: State<AppState>, ids: Vec<i64>, kind: String) -> CmdResult<usize> {
    let mut conn = state.db.conn().map_err(err)?;
    let n = media_core::cleanup::dismiss(&mut conn, &ids, &kind).map_err(err)?;
    state.changed();
    Ok(n)
}

#[tauri::command(async)]
fn undismiss(state: State<AppState>, ids: Vec<i64>, kind: String) -> CmdResult<usize> {
    let conn = state.db.conn().map_err(err)?;
    media_core::cleanup::undismiss(&conn, &ids, &kind).map_err(err)
}

// ---------------------------------------------------------------------------------------------
// Background analysis
// ---------------------------------------------------------------------------------------------

#[tauri::command(async)]
fn analysis_status(app: AppHandle, state: State<AppState>) -> LaneStatus {
    state.lane.status(&app)
}

#[tauri::command(async)]
fn analysis_pause(app: AppHandle, state: State<AppState>, paused: bool) {
    state.lane.set_user_paused(&app, paused);
}

#[tauri::command(async)]
fn analysis_start(app: AppHandle, state: State<AppState>) {
    state.lane.set_user_paused(&app, false);
    // A failed face engine is loaded again (e.g. after the AI files were put in place).
    state.lane.reset_engine();
    state.lane.kick(&app, true);
}

#[tauri::command(async)]
fn get_analysis_options(state: State<AppState>) -> CmdResult<AnalysisOptions> {
    let conn = state.db.conn().map_err(err)?;
    analysis::load_options(&conn).map_err(err)
}

#[tauri::command(async)]
fn set_analysis_options(app: AppHandle, state: State<AppState>, options: AnalysisOptions) -> CmdResult<()> {
    let conn = state.db.conn().map_err(err)?;
    let before = analysis::load_options(&conn).map_err(err)?;
    analysis::save_options(&conn, &options).map_err(err)?;
    if before.people != options.people || before.gpu != options.gpu {
        state.lane.reset_engine();
    }
    // Turning people on (or auto back on) starts work right away.
    if (options.people && !before.people) || (options.auto && !before.auto) {
        state.lane.kick(&app, true);
    }
    Ok(())
}

#[tauri::command(async)]
fn start_thumbs(app: AppHandle, state: State<AppState>) -> CmdResult<()> {
    spawn_job(&app, &state, "thumbs", |st, progress| thumbs::warm(&st.db, &st.thumbs, &st.stop, progress))
}

#[tauri::command(async)]
fn cancel_job(state: State<AppState>) {
    state.stop.stop();
}

// ---------------------------------------------------------------------------------------------
// Library browsing
// ---------------------------------------------------------------------------------------------

#[tauri::command(async)]
fn library_stats(state: State<AppState>) -> CmdResult<LibraryStats> {
    let conn = state.db.conn().map_err(err)?;
    db::library_stats(&conn).map_err(err)
}

#[tauri::command(async)]
fn timeline(state: State<AppState>, filter: FileFilter) -> CmdResult<Vec<MonthCount>> {
    let conn = state.db.conn().map_err(err)?;
    db::timeline_summary(&conn, &filter).map_err(err)
}

#[tauri::command(async)]
fn filter_options(state: State<AppState>) -> CmdResult<FilterOptions> {
    let conn = state.db.conn().map_err(err)?;
    db::filter_options(&conn).map_err(err)
}

#[tauri::command(async)]
fn filtered_stats(state: State<AppState>, filter: FileFilter) -> CmdResult<FilteredStats> {
    let conn = state.db.conn().map_err(err)?;
    db::filtered_stats(&conn, &filter).map_err(err)
}

#[tauri::command(async)]
fn group_summary(state: State<AppState>, filter: FileFilter, group_by: String) -> CmdResult<Vec<GroupRow>> {
    let conn = state.db.conn().map_err(err)?;
    db::group_summary(&conn, &filter, &group_by).map_err(err)
}

#[tauri::command(async)]
fn grid(state: State<AppState>, filter: FileFilter, offset: i64, limit: i64) -> CmdResult<Vec<GridItem>> {
    let conn = state.db.conn().map_err(err)?;
    db::query_grid(&conn, &filter, offset, limit.min(5000)).map_err(err)
}

#[tauri::command(async)]
fn query_ids(state: State<AppState>, filter: FileFilter) -> CmdResult<Vec<i64>> {
    let conn = state.db.conn().map_err(err)?;
    db::query_ids(&conn, &filter).map_err(err)
}

#[derive(Serialize)]
struct FileDetail {
    file: FileRow,
    /// Why this file is in cleanup suggestions: (kind, detail).
    reasons: Vec<(String, String)>,
    labels: Vec<LabelRow>,
    abs_path: String,
    taken: String,
    confidence: String,
    thumb_cached: bool,
    /// "Seattle, Washington" when the file has GPS.
    place: Option<String>,
    /// Named people in the photo.
    people: Vec<(i64, String)>,
    /// "original" / "copy" and how many identical files there are, when there are copies.
    dup_role: Option<String>,
    dup_group_size: i64,
}

#[tauri::command(async)]
fn file_detail(state: State<AppState>, id: i64) -> CmdResult<FileDetail> {
    let conn = state.db.conn().map_err(err)?;
    detail_of(&state, &conn, id)
}

#[derive(Serialize)]
struct CompareInfo {
    files: Vec<FileDetail>,
    #[serde(flatten)]
    comparison: media_core::compare::Comparison,
}

/// "This copy is the original" (one per group of identical files).
#[tauri::command(async)]
fn set_original(state: State<AppState>, id: i64) -> CmdResult<()> {
    let mut conn = state.db.conn().map_err(err)?;
    media_core::identity::set_original(&mut conn, id).map_err(err)?;
    state.changed();
    Ok(())
}

#[tauri::command(async)]
fn get_dup_rules(state: State<AppState>) -> CmdResult<dedupe::DupRules> {
    let conn = state.db.conn().map_err(err)?;
    dedupe::DupRules::load(&conn).map_err(err)
}

/// Save the duplicate keep rules and re-pick keepers; returns how many groups changed.
#[tauri::command(async)]
fn set_dup_rules(state: State<AppState>, rules: dedupe::DupRules) -> CmdResult<usize> {
    no_job(&state)?;
    let mut conn = state.db.conn().map_err(err)?;
    rules.save(&conn).map_err(err)?;
    state.changed();
    let changed = dedupe::apply_rules(&mut conn).map_err(err)?;
    media_core::identity::refresh_roles(&mut conn).map_err(err)?;
    Ok(changed)
}

/// Folders worth a "delete from here first" rule: messenger / download folders that hold
/// duplicate copies, most copies first.
#[tauri::command(async)]
fn dup_rule_suggestions(state: State<AppState>) -> CmdResult<Vec<(String, i64)>> {
    let conn = state.db.conn().map_err(err)?;
    let sql = format!(
        "SELECT v.mount, {} d, COUNT(*) n FROM dup_members m JOIN dup_groups g ON g.id = m.group_id AND g.resolved = 0
           JOIN files f ON f.id = m.file_id JOIN volumes v ON v.id = f.volume_id
         WHERE f.missing = 0 AND f.quarantined = 0
           AND EXISTS (SELECT 1 FROM labels l WHERE l.file_id = f.id AND l.label LIKE 'src:%')
         GROUP BY f.volume_id, d ORDER BY n DESC LIMIT 5",
        db::DIR_SQL
    );
    let mut st = conn.prepare(&sql).map_err(err)?;
    let rows = st
        .query_map([], |r| {
            let mount: String = r.get(0)?;
            let dir: String = r.get(1)?;
            Ok((volumes::join_rel(Path::new(&mount), &dir).to_string_lossy().into_owned(), r.get::<_, i64>(2)?))
        })
        .map_err(err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?;
    Ok(rows)
}

/// Photos that look like this one (resized / recompressed / edited copies), most similar first.
#[tauri::command(async)]
fn similar_to(state: State<AppState>, id: i64) -> CmdResult<Vec<i64>> {
    let conn = state.db.conn().map_err(err)?;
    media_core::compare::similar_to(&conn, id, 3).map_err(err)
}

/// Before the backup check: fingerprint older entries that could have a copy (once).
#[tauri::command]
async fn prepare_copies(app: AppHandle) -> CmdResult<usize> {
    tauri::async_runtime::spawn_blocking(move || {
        let st = app.state::<AppState>();
        let conn = st.db.conn().map_err(err)?;
        volumes::refresh(&conn).map_err(err)?;
        // A few seconds here (the filter works with what is known); the rest goes on behind.
        let started = std::time::Instant::now();
        let mut done = 0;
        while started.elapsed() < std::time::Duration::from_secs(3) {
            let n = media_core::identity::backfill_prehashes_with(&conn, 300, &StopFlag::new(), &|| started.elapsed() >= std::time::Duration::from_secs(3)).map_err(err)?;
            done += n;
            if n == 0 {
                break;
            }
        }
        st.lane.kick_backfill(&app);
        Ok(done)
    })
    .await
    .map_err(err)?
}

#[derive(Clone, Serialize)]
struct DriveConnected {
    volume_id: i64,
    label: String,
}

/// Notice drives being plugged in (every few seconds) so the UI can offer "check for new files".
/// Only drives that hold a registered folder are announced.
fn watch_drives(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let mut known: Option<std::collections::HashSet<i64>> = None;
        let mut known_cards: Option<std::collections::HashSet<String>> = None;
        loop {
            let online = (|| -> anyhow::Result<(std::collections::HashSet<i64>, Vec<(i64, String)>)> {
                let st = app.state::<AppState>();
                let conn = st.db.conn()?;
                volumes::refresh(&conn)?;
                let mut set = std::collections::HashSet::new();
                let mut labels = Vec::new();
                for s in db::list_sources(&conn)?.into_iter().filter(|s| s.online) {
                    if set.insert(s.volume_id) {
                        labels.push((s.volume_id, s.volume_label));
                    }
                }
                Ok((set, labels))
            })();
            // Camera cards (removable, with DCIM), registered or not: offer to import.
            let cards: std::collections::HashSet<String> = media_core::importer::find_cards().into_iter().map(|c| c.mount).collect();
            if let Some(prev) = &known_cards {
                for c in media_core::importer::find_cards().into_iter().filter(|c| !prev.contains(&c.mount)) {
                    let _ = app.emit("card-connected", c);
                }
            }
            known_cards = Some(cards);
            if let Ok((set, labels)) = online {
                if let Some(prev) = &known {
                    // A camera card being imported has its own offer; no "check for new files" too.
                    let card_ids: std::collections::HashSet<i64> = app
                        .state::<AppState>()
                        .db
                        .conn()
                        .ok()
                        .map(|c| {
                            known_cards.iter().flatten().filter_map(|m| c.query_row("SELECT id FROM volumes WHERE mount = ?", [m], |r| r.get(0)).ok()).collect()
                        })
                        .unwrap_or_default();
                    for (id, label) in labels.into_iter().filter(|(id, _)| !prev.contains(id) && !card_ids.contains(id)) {
                        let _ = app.emit("drive-connected", DriveConnected { volume_id: id, label });
                    }
                }
                known = Some(set);
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    });
}

/// Several files side by side: details, which are byte-identical, which one to keep.
#[tauri::command]
async fn compare_files(app: AppHandle, ids: Vec<i64>) -> CmdResult<CompareInfo> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let conn = state.db.conn().map_err(err)?;
        let files = ids.iter().map(|id| detail_of(&state, &conn, *id)).collect::<CmdResult<Vec<_>>>()?;
        let comparison = media_core::compare::compare(&conn, &ids).map_err(err)?;
        Ok(CompareInfo { files, comparison })
    })
    .await
    .map_err(err)?
}

fn detail_of(state: &AppState, conn: &media_core::rusqlite::Connection, id: i64) -> CmdResult<FileDetail> {
    let f = db::get_file(conn, id).map_err(err)?.ok_or("not found")?;
    Ok(FileDetail {
        reasons: media_core::cleanup::reasons(conn, id).map_err(err)?,
        labels: labels::labels_for(conn, id).map_err(err)?,
        abs_path: f.abs_path().to_string_lossy().into(),
        taken: f.taken_at.map(meta::fmt_ts).unwrap_or_default(),
        confidence: meta::confidence(&f.date_source).into(),
        thumb_cached: state.thumbs.has_thumb(&f),
        place: media_core::places::place_of(conn, id).map_err(err)?,
        people: media_core::people::names_in(conn, id).map_err(err)?,
        dup_role: conn.query_row("SELECT role FROM dup_roles WHERE file_id = ?", [id], |r| r.get(0)).ok(),
        dup_group_size: conn.query_row("SELECT group_size FROM dup_roles WHERE file_id = ?", [id], |r| r.get(0)).unwrap_or(0),
        file: f,
    })
}

#[tauri::command]
async fn preview_path(app: AppHandle, id: i64) -> CmdResult<String> {
    tauri::async_runtime::spawn_blocking(move || {
        let st = app.state::<AppState>();
        let conn = st.db.conn().map_err(err)?;
        let f = db::get_file(&conn, id).map_err(err)?.ok_or("not found")?;
        st.thumbs.ensure_preview(&f).map(|p| p.to_string_lossy().into_owned()).map_err(err)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
async fn video_preview_path(app: AppHandle, id: i64) -> CmdResult<String> {
    tauri::async_runtime::spawn_blocking(move || {
        let st = app.state::<AppState>();
        let conn = st.db.conn().map_err(err)?;
        let f = db::get_file(&conn, id).map_err(err)?.ok_or("not found")?;
        st.thumbs.ensure_video_preview(&f).map(|p| p.to_string_lossy().into_owned()).map_err(err)
    })
    .await
    .map_err(err)?
}

#[tauri::command(async)]
fn reveal_file(app: AppHandle, state: State<AppState>, id: i64) -> CmdResult<()> {
    let conn = state.db.conn().map_err(err)?;
    let f = db::get_file(&conn, id).map_err(err)?.ok_or("not found")?;
    app.opener().reveal_item_in_dir(f.abs_path()).map_err(err)
}

#[tauri::command(async)]
fn open_file(app: AppHandle, state: State<AppState>, id: i64) -> CmdResult<()> {
    let conn = state.db.conn().map_err(err)?;
    let f = db::get_file(&conn, id).map_err(err)?.ok_or("not found")?;
    app.opener().open_path(f.abs_path().to_string_lossy(), None::<&str>).map_err(err)
}

// ---------------------------------------------------------------------------------------------
// VLC (external player for formats the webview can't play)
// ---------------------------------------------------------------------------------------------

fn find_vlc() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
            if let Some(p) = std::env::var_os(var) {
                cands.push(PathBuf::from(p).join("VideoLAN").join("VLC").join("vlc.exe"));
            }
        }
        if let Some(p) = std::env::var_os("LOCALAPPDATA") {
            cands.push(PathBuf::from(p).join("Programs").join("VideoLAN").join("VLC").join("vlc.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    cands.push(PathBuf::from("/Applications/VLC.app/Contents/MacOS/VLC"));
    let exe = if cfg!(windows) { "vlc.exe" } else { "vlc" };
    if let Some(path) = std::env::var_os("PATH") {
        cands.extend(std::env::split_paths(&path).map(|d| d.join(exe)));
    }
    cands.into_iter().find(|p| p.is_file())
}

#[tauri::command(async)]
fn vlc_path() -> Option<String> {
    find_vlc().map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command(async)]
fn open_in_vlc(state: State<AppState>, id: i64) -> CmdResult<()> {
    let vlc = find_vlc().ok_or("VLC를 찾을 수 없습니다. VLC가 설치되어 있는지 확인하세요")?;
    let conn = state.db.conn().map_err(err)?;
    let f = db::get_file(&conn, id).map_err(err)?.ok_or("not found")?;
    if !f.online {
        return Err("드라이브가 연결되어 있지 않습니다".into());
    }
    std::process::Command::new(vlc).arg(f.abs_path()).spawn().map_err(err)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Auto-update (GitHub Releases + signed installers)
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
struct UpdateInfo {
    current: String,
    available: bool,
    version: Option<String>,
    notes: Option<String>,
    date: Option<String>,
}

#[derive(Clone, Serialize)]
struct UpdateProgress {
    downloaded: u64,
    total: Option<u64>,
}

/// `MO_UPDATE_ENDPOINT` overrides the configured feed (for testing a release before publishing).
fn updater(app: &AppHandle) -> Result<tauri_plugin_updater::Updater, String> {
    let mut b = app.updater_builder();
    if let Ok(url) = std::env::var("MO_UPDATE_ENDPOINT") {
        b = b.endpoints(vec![url.parse::<tauri::Url>().map_err(err)?]).map_err(err)?;
    }
    b.build().map_err(err)
}

#[tauri::command]
async fn check_update(app: AppHandle) -> CmdResult<UpdateInfo> {
    let current = app.package_info().version.to_string();
    let u = updater(&app)?.check().await.map_err(err)?;
    Ok(match u {
        Some(u) => UpdateInfo { current, available: true, version: Some(u.version.clone()), notes: u.body.clone(), date: u.date.map(|d| d.to_string()) },
        None => UpdateInfo { current, available: false, version: None, notes: None, date: None },
    })
}

/// Download, verify (signature), install and restart. Refused while a job is moving files.
#[tauri::command]
async fn install_update(app: AppHandle) -> CmdResult<()> {
    {
        let st = app.state::<AppState>();
        let mut j = st.job.locked();
        if let Some(cur) = j.as_ref() {
            return Err(format!("작업이 끝난 뒤 업데이트할 수 있습니다 (진행 중: {cur})"));
        }
        *j = Some("update".into());
    }
    let release = |app: &AppHandle| *app.state::<AppState>().job.locked() = None;
    let update = match updater(&app)?.check().await {
        Ok(Some(u)) => u,
        Ok(None) => {
            release(&app);
            return Err("이미 최신 버전입니다".into());
        }
        Err(e) => {
            release(&app);
            return Err(err(e));
        }
    };
    let emit_app = app.clone();
    let mut downloaded: u64 = 0;
    let res = update
        .download_and_install(
            move |chunk, total| {
                downloaded += chunk as u64;
                let _ = emit_app.emit("update-progress", UpdateProgress { downloaded, total });
            },
            || {},
        )
        .await;
    if let Err(e) = res {
        release(&app);
        let msg = e.to_string();
        return Err(if msg.contains("signature") {
            "업데이트 파일의 서명이 맞지 않아 설치하지 않았습니다 (손상되었거나 위조된 파일)".into()
        } else if msg.contains("https") {
            "업데이트 주소가 안전하지 않습니다 (https 필요)".into()
        } else {
            format!("업데이트 설치 실패: {msg}")
        });
    }
    app.restart();
}

// ---------------------------------------------------------------------------------------------
// Labels & dates
// ---------------------------------------------------------------------------------------------

#[tauri::command(async)]
fn label_counts(state: State<AppState>) -> CmdResult<Vec<LabelCount>> {
    let conn = state.db.conn().map_err(err)?;
    labels::label_counts(&conn).map_err(err)
}

#[tauri::command(async)]
fn add_label(state: State<AppState>, ids: Vec<i64>, label: String) -> CmdResult<usize> {
    let mut conn = state.db.conn().map_err(err)?;
    labels::add_user_label(&mut conn, &ids, &label).map_err(err)
}

#[tauri::command(async)]
fn remove_label(state: State<AppState>, ids: Vec<i64>, label: String) -> CmdResult<usize> {
    let mut conn = state.db.conn().map_err(err)?;
    labels::remove_label(&mut conn, &ids, &label).map_err(err)
}

/// `datetime` is local wall time, "YYYY-MM-DDTHH:MM" (from an <input type="datetime-local">).
#[tauri::command(async)]
fn set_date(state: State<AppState>, ids: Vec<i64>, datetime: String) -> CmdResult<usize> {
    let dt = chrono::NaiveDateTime::parse_from_str(&datetime, "%Y-%m-%dT%H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(&datetime, "%Y-%m-%dT%H:%M:%S"))
        .map_err(err)?;
    let mut conn = state.db.conn().map_err(err)?;
    let n = labels::set_taken_at(&mut conn, &ids, meta::to_ts(dt)).map_err(err)?;
    media_core::places::refresh(&mut conn).map_err(err)?;
    Ok(n)
}

#[tauri::command(async)]
fn shift_date(state: State<AppState>, ids: Vec<i64>, delta_secs: i64) -> CmdResult<usize> {
    let mut conn = state.db.conn().map_err(err)?;
    let n = labels::shift_taken_at(&mut conn, &ids, delta_secs).map_err(err)?;
    media_core::places::refresh(&mut conn).map_err(err)?;
    Ok(n)
}

// ---------------------------------------------------------------------------------------------
// Duplicates
// ---------------------------------------------------------------------------------------------

#[tauri::command(async)]
fn start_dedupe(app: AppHandle, state: State<AppState>, kind: DupKind, similarity: Option<Similarity>) -> CmdResult<()> {
    spawn_job(&app, &state, &format!("dedupe:{}", kind.as_str()), move |st, progress| {
        dedupe::run(&st.db, kind, similarity.unwrap_or(Similarity::High), &st.stop, progress)
    })
}

#[tauri::command(async)]
fn dup_summary(state: State<AppState>) -> CmdResult<Vec<DupSummary>> {
    let conn = state.db.conn().map_err(err)?;
    dedupe::summary(&conn).map_err(err)
}

#[tauri::command(async)]
fn dup_groups(state: State<AppState>, kind: String, offset: i64, limit: i64) -> CmdResult<Vec<DupGroup>> {
    let conn = state.db.conn().map_err(err)?;
    dedupe::list_groups(&conn, &kind, offset, limit).map_err(err)
}

#[tauri::command(async)]
fn set_keep(state: State<AppState>, group_id: i64, file_id: i64, keep: bool) -> CmdResult<()> {
    no_job(&state)?;
    let conn = state.db.conn().map_err(err)?;
    dedupe::set_keep(&conn, group_id, file_id, keep).map_err(err)?;
    state.changed();
    Ok(())
}

#[tauri::command(async)]
fn start_dedupe_apply(app: AppHandle, state: State<AppState>, kind: String, group_ids: Option<Vec<i64>>) -> CmdResult<()> {
    spawn_job(&app, &state, "dedupe:apply", move |st, progress| dedupe::apply(&st.db, &kind, group_ids, &st.stop, progress))
}

// ---------------------------------------------------------------------------------------------
// Organize
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
struct PlanView {
    id: String,
    summary: PlanSummary,
    options: OrganizeOptions,
}

#[tauri::command(async)]
fn start_plan(app: AppHandle, state: State<AppState>, options: OrganizeOptions) -> CmdResult<()> {
    spawn_job(&app, &state, "organize:plan", move |st, progress| {
        let plan = organizer::plan(&st.db, &options, &st.stop, progress)?;
        let view = PlanView { id: plan.id.clone(), summary: plan.summary.clone(), options: plan.options.clone() };
        let mut plans = st.plans.locked();
        plans.clear();
        plans.insert(plan.id.clone(), Arc::new(plan));
        Ok(view)
    })
}

#[tauri::command(async)]
fn plan_items(state: State<AppState>, plan_id: String, action: Option<String>, offset: usize, limit: usize) -> CmdResult<Vec<PlanItem>> {
    let plans = state.plans.locked();
    let plan = plans.get(&plan_id).ok_or("계획이 만료되었습니다. 다시 미리보기를 실행하세요")?;
    let want = action.as_deref();
    Ok(plan
        .items
        .iter()
        .filter(|i| match want {
            None => true,
            Some(a) => serde_json::to_value(i.action).ok().and_then(|v| v.as_str().map(|s| s == a)).unwrap_or(false),
        })
        .skip(offset)
        .take(limit)
        .cloned()
        .collect())
}

#[tauri::command(async)]
fn start_execute(
    app: AppHandle,
    state: State<AppState>,
    plan_id: String,
    groups: Option<Vec<String>>,
    year: Option<String>,
    purge: Option<bool>,
    drop_ids: Option<Vec<i64>>,
    decisions: Option<Vec<media_core::triage::Decision>>,
) -> CmdResult<()> {
    let plan = state.plans.locked().get(&plan_id).cloned().ok_or("계획이 만료되었습니다. 다시 미리보기를 실행하세요")?;
    spawn_job(&app, &state, "organize:run", move |st, progress| {
        // Only some folders ("try one first") or one year: re-plan afterwards for the rest.
        let xo = organizer::ExecOptions {
            purge: purge.unwrap_or(false),
            label: year.as_deref().map(organizer::year_label),
            drop_ids: drop_ids.unwrap_or_default(),
            decisions: decisions.unwrap_or_default(),
        };
        let part = match (&groups, &year) {
            (Some(g), _) => organizer::subset(&plan, g),
            (None, Some(y)) => organizer::subset_year(&plan, y),
            (None, None) => (*plan).clone(),
        };
        let r = organizer::execute_with(&st.db, &part, &xo, &st.stop, progress)?;
        st.plans.locked().remove(&plan.id);
        Ok(r)
    })
}

fn with_plan<T>(state: &State<AppState>, plan_id: &str, f: impl FnOnce(&Plan) -> T) -> CmdResult<T> {
    let plans = state.plans.locked();
    let plan = plans.get(plan_id).ok_or("계획이 만료되었습니다. 다시 미리보기를 실행하세요")?;
    Ok(f(plan))
}

/// The library plan by year ("한 해씩").
#[tauri::command(async)]
fn library_years(state: State<AppState>, plan_id: String) -> CmdResult<Vec<organizer::YearBucket>> {
    with_plan(&state, &plan_id, organizer::year_buckets)
}

#[tauri::command(async)]
fn year_copies(state: State<AppState>, plan_id: String, year: String) -> CmdResult<Vec<organizer::CopyFolder>> {
    with_plan(&state, &plan_id, |p| organizer::year_copies(p, &year))
}

#[tauri::command(async)]
fn year_copy_pairs(state: State<AppState>, plan_id: String, year: String, dir: String, offset: usize, limit: usize) -> CmdResult<Vec<organizer::CopyPair>> {
    with_plan(&state, &plan_id, |p| organizer::year_copy_pairs(p, &year, &dir, offset, limit))
}

#[tauri::command(async)]
fn year_keep(state: State<AppState>, plan_id: String, year: String, offset: usize, limit: usize) -> CmdResult<Vec<organizer::KeepItem>> {
    with_plan(&state, &plan_id, |p| organizer::year_keep(p, &year, offset, limit))
}

#[derive(Serialize)]
struct DiskSpace {
    free: u64,
    total: u64,
    /// Always left free on the library drive.
    reserve: u64,
}

/// One year's bursts / similar photos to review (other drives and edited versions locked).
#[tauri::command(async)]
fn year_triage_groups(
    state: State<AppState>,
    plan_id: String,
    year: String,
    kind: String,
    offset: usize,
    limit: usize,
) -> CmdResult<Vec<media_core::triage::TriageGroup>> {
    let (files, lib) = with_plan(&state, &plan_id, |p| (organizer::year_files(p, &year), organizer::lib_vol(p)))?;
    let conn = state.db.conn().map_err(err)?;
    media_core::triage::groups_in(&conn, &kind, &files, lib.unwrap_or(-1), offset, limit).map_err(err)
}

#[derive(Serialize)]
struct YearTriageCounts {
    bursts: usize,
    similar: usize,
    /// Photos of the year not analysed yet (not in any group so far).
    unanalyzed: usize,
}

#[tauri::command(async)]
fn year_triage_counts(state: State<AppState>, plan_id: String, year: String) -> CmdResult<YearTriageCounts> {
    let (files, lib) = with_plan(&state, &plan_id, |p| (organizer::year_files(p, &year), organizer::lib_vol(p).unwrap_or(-1)))?;
    let conn = state.db.conn().map_err(err)?;
    let unanalyzed: std::collections::HashSet<i64> = {
        let mut st = conn
            .prepare("SELECT f.id FROM files f WHERE f.kind = 'photo' AND NOT EXISTS (SELECT 1 FROM analysis a WHERE a.file_id = f.id)")
            .map_err(err)?;
        let rows = st.query_map([], |r| r.get(0)).map_err(err)?.collect::<Result<_, _>>().map_err(err)?;
        rows
    };
    Ok(YearTriageCounts {
        bursts: media_core::triage::count_in(&conn, "burst", &files, lib).map_err(err)?,
        similar: media_core::triage::count_in(&conn, "similar_image", &files, lib).map_err(err)?,
        unanalyzed: files.iter().filter(|id| unanalyzed.contains(id)).count(),
    })
}

/// Files planned into the chosen library folders (preview of a selection).
#[tauri::command(async)]
fn group_items(state: State<AppState>, plan_id: String, groups: Vec<String>, offset: usize, limit: usize) -> CmdResult<Vec<organizer::KeepItem>> {
    with_plan(&state, &plan_id, |p| organizer::group_items(p, &groups, offset, limit))
}

#[tauri::command(async)]
fn get_library_merges(state: State<AppState>) -> CmdResult<Vec<media_core::library::Merge>> {
    let conn = state.db.conn().map_err(err)?;
    media_core::library::load_merges(&conn).map_err(err)
}

#[tauri::command(async)]
fn set_library_merges(state: State<AppState>, merges: Vec<media_core::library::Merge>) -> CmdResult<()> {
    let conn = state.db.conn().map_err(err)?;
    media_core::library::save_merges(&conn, &merges).map_err(err)
}

/// How many deleted photos are kept out of the library (their copies elsewhere not brought back).
#[tauri::command(async)]
fn library_excluded_count(state: State<AppState>) -> CmdResult<usize> {
    let conn = state.db.conn().map_err(err)?;
    Ok(media_core::library::load_excluded(&conn).map_err(err)?.len())
}

#[tauri::command(async)]
fn clear_library_excluded(state: State<AppState>) -> CmdResult<()> {
    let conn = state.db.conn().map_err(err)?;
    media_core::library::save_excluded(&conn, &[]).map_err(err)
}

#[derive(Serialize)]
struct FolderDeletePreview {
    count: usize,
    bytes: i64,
    not_copied: u64,
    /// ♥ / original / keep folder (the last copy does not count: deleting it is the point).
    protected: Vec<media_core::protect::Blocked>,
}

#[tauri::command(async)]
fn library_delete_preview(state: State<AppState>, plan_id: String, groups: Vec<String>) -> CmdResult<FolderDeletePreview> {
    let t = with_plan(&state, &plan_id, |p| organizer::delete_targets(p, &groups))?;
    let conn = state.db.conn().map_err(err)?;
    let protected = media_core::protect::check(&conn, &t.ids).map_err(err)?.into_iter().filter(|b| b.reason != "last_copy").collect();
    Ok(FolderDeletePreview { count: t.ids.len(), bytes: t.bytes, not_copied: t.not_copied, protected })
}

/// Delete library folders for good (see `organizer::delete_folders`).
#[tauri::command(async)]
fn start_library_delete(app: AppHandle, state: State<AppState>, plan_id: String, groups: Vec<String>, keep_protected: bool) -> CmdResult<()> {
    let plan = state.plans.locked().get(&plan_id).cloned().ok_or("계획이 만료되었습니다. 다시 미리보기를 실행하세요")?;
    spawn_job(&app, &state, "library:delete", move |st, progress| {
        let r = organizer::delete_folders(&st.db, &plan, &groups, keep_protected, &st.stop, progress)?;
        st.plans.locked().remove(&plan.id);
        Ok(r)
    })
}

/// Folders copied whole, on the library drive ("100% 복사 폴더"): reads the index only.
#[tauri::command(async)]
fn copy_folders(state: State<'_, AppState>, near: Option<f64>, refresh: Option<bool>) -> CmdResult<media_core::folders::Found> {
    if refresh.unwrap_or(false) {
        state.changed();
    }
    let near = near.unwrap_or(media_core::folders::NEAR);
    let gen = state.data_gen.load(std::sync::atomic::Ordering::Relaxed);
    state
        .folders_cache
        .get(gen, near, || {
            let conn = state.db.conn()?;
            media_core::folders::find_with(&conn, near)
        })
        .map_err(err)
}

/// A folder's photos and videos, sub-folder by sub-folder.
#[tauri::command(async)]
fn folder_files(state: State<'_, AppState>, folder: media_core::folders::FolderRef) -> CmdResult<Vec<media_core::folders::FolderFiles>> {
    let conn = state.db.conn().map_err(err)?;
    media_core::folders::files_in(&conn, &folder).map_err(err)
}

/// Two folders side by side: what both have, what only one has.
#[tauri::command(async)]
fn folder_compare(state: State<'_, AppState>, keep: media_core::folders::FolderRef, copy: media_core::folders::FolderRef) -> CmdResult<media_core::folders::Compare> {
    let conn = state.db.conn().map_err(err)?;
    media_core::folders::compare(&conn, &keep, &copy).map_err(err)
}

/// The folders of a set photo by photo, for looking at them side by side.
#[tauri::command(async)]
fn folder_align(state: State<'_, AppState>, keep: media_core::folders::FolderRef, copies: Vec<media_core::folders::FolderRef>) -> CmdResult<Vec<media_core::folders::AlignRow>> {
    let conn = state.db.conn().map_err(err)?;
    media_core::folders::align(&conn, &keep, &copies).map_err(err)
}

/// Compare two files to the end: where they first differ, if they do (the index then learns
/// that they are different files).
#[tauri::command(async)]
fn folder_first_difference(state: State<'_, AppState>, a: i64, b: i64, ticket: Option<u64>) -> CmdResult<media_core::folders::FirstDifference> {
    use std::sync::atomic::Ordering;
    // With a ticket (the photo in view): the newest ticket wins, an older comparison gives up.
    if let Some(t) = ticket {
        state.walk_ticket.fetch_max(t, Ordering::Relaxed);
    }
    let cancelled = || ticket.is_some_and(|t| state.walk_ticket.load(Ordering::Relaxed) != t);
    let conn = state.db.conn().map_err(err)?;
    let d = media_core::folders::check_pair_with(&conn, a, b, &cancelled).map_err(err)?;
    if d.at.is_some() {
        state.changed(); // the two are known as different files now: lists change
    }
    Ok(d)
}

/// The side-by-side window moved on: whatever comparison it had asked for gives up.
#[tauri::command(async)]
fn cancel_folder_check(state: State<'_, AppState>, ticket: u64) {
    state.walk_ticket.fetch_max(ticket, std::sync::atomic::Ordering::Relaxed);
}

/// Take the chosen files of copy folders into the folder that stays (replace or add).
#[tauri::command(async)]
fn start_folder_swap(app: AppHandle, state: State<AppState>, keep: media_core::folders::FolderRef, groups: Vec<media_core::folders::SwapGroup>) -> CmdResult<()> {
    spawn_job(&app, &state, "cleanup:swap", move |st, progress| {
        let r = media_core::folders::swap(&st.db, &keep, &groups, &st.stop, progress)?;
        st.plans.locked().clear();
        Ok(r)
    })
}

/// Sets the user has looked through.
#[tauri::command(async)]
fn folders_reviewed(state: State<AppState>) -> CmdResult<Vec<String>> {
    let conn = state.db.conn().map_err(err)?;
    media_core::folders::reviewed(&conn).map_err(err)
}

#[tauri::command(async)]
fn set_folder_reviewed(state: State<AppState>, key: String, on: bool) -> CmdResult<Vec<String>> {
    let _one = state.settings_lock.locked();
    let conn = state.db.conn().map_err(err)?;
    media_core::folders::set_reviewed(&conn, &key, on).map_err(err)
}

/// What else a folder holds on disk (files that are not photos or videos).
#[tauri::command(async)]
fn folder_props(state: State<'_, AppState>, folder: media_core::folders::FolderRef) -> CmdResult<media_core::folders::FolderProps> {
    let conn = state.db.conn().map_err(err)?;
    media_core::folders::props(&conn, &folder).map_err(err)
}

/// Remove the chosen copy folders (each file compared with its twin before it is deleted).
#[tauri::command(async)]
fn start_remove_copy_folders(app: AppHandle, state: State<AppState>, picks: Vec<media_core::folders::Pick>, purge: bool, quick: Option<bool>) -> CmdResult<()> {
    let opts = media_core::folders::RemoveOptions { purge, quick: quick.unwrap_or(false) };
    spawn_job(&app, &state, "cleanup:folders", move |st, progress| {
        let r = media_core::folders::remove_with(&st.db, &picks, &opts, &st.stop, progress)?;
        // Plans made before no longer match what is on the drive.
        st.plans.locked().clear();
        Ok(r)
    })
}

#[tauri::command(async)]
fn open_folder(app: AppHandle, path: String) -> CmdResult<()> {
    if !Path::new(&path).is_dir() {
        return Err("폴더를 찾을 수 없습니다".into());
    }
    app.opener().open_path(path, None::<&str>).map_err(err)
}

/// The files that are the same photo as this one: where the original is and what the others are.
#[tauri::command(async)]
fn file_family(state: State<AppState>, id: i64) -> CmdResult<Option<media_core::identity::Family>> {
    let conn = state.db.conn().map_err(err)?;
    media_core::identity::family(&conn, id).map_err(err)
}

/// Face finding on / off for this session.
#[tauri::command(async)]
fn set_faces_enabled(app: AppHandle, state: State<AppState>, on: bool) -> CmdResult<()> {
    state.lane.set_faces_enabled(&app, on);
    Ok(())
}

#[tauri::command(async)]
fn disk_space(path: String) -> CmdResult<Option<DiskSpace>> {
    Ok(media_core::volumes::disk_space(Path::new(&path))
        .map(|(free, total)| DiskSpace { free, total, reserve: media_core::volumes::space_reserve(total) }))
}

/// The library plan as destination folders.
#[tauri::command(async)]
fn plan_tree(state: State<AppState>, plan_id: String) -> CmdResult<Vec<organizer::TreeNode>> {
    let plans = state.plans.locked();
    let plan = plans.get(&plan_id).ok_or("계획이 만료되었습니다. 다시 미리보기를 실행하세요")?;
    Ok(organizer::plan_tree(plan))
}

#[tauri::command(async)]
fn get_library_rules(state: State<AppState>) -> CmdResult<Vec<media_core::library::FolderRule>> {
    let conn = state.db.conn().map_err(err)?;
    media_core::library::load_rules(&conn).map_err(err)
}

/// Remember a correction for a source folder (event name, not an event, leave it out).
#[tauri::command(async)]
fn set_library_rules(state: State<AppState>, rules: Vec<media_core::library::FolderRule>) -> CmdResult<()> {
    let conn = state.db.conn().map_err(err)?;
    media_core::library::save_rules(&conn, &rules).map_err(err)
}

// ---------------------------------------------------------------------------------------------
// History (undo / purge)
// ---------------------------------------------------------------------------------------------

#[tauri::command(async)]
fn batches(state: State<AppState>) -> CmdResult<Vec<BatchRow>> {
    let conn = state.db.conn().map_err(err)?;
    journal::list_batches(&conn).map_err(err)
}

#[tauri::command(async)]
fn start_undo(app: AppHandle, state: State<AppState>, batch_id: String) -> CmdResult<()> {
    spawn_job(&app, &state, "undo", move |st, progress| {
        let conn = st.db.conn()?;
        volumes::refresh(&conn)?;
        journal::undo_batch_with(&conn, &batch_id, &st.stop, progress)
    })
}

/// Delete (quarantine) the selected files as one undoable batch.
#[tauri::command(async)]
fn start_delete(app: AppHandle, state: State<AppState>, ids: Vec<i64>, force: Option<bool>, label: Option<String>) -> CmdResult<()> {
    if ids.is_empty() {
        return Err("선택한 파일이 없습니다".into());
    }
    let force = force.unwrap_or(false);
    spawn_job(&app, &state, "delete", move |st, progress| journal::delete_files(&st.db, &ids, force, label.as_deref(), &st.stop, progress))
}

/// Groups to review quickly (bursts, copies, versions), with the suggested keepers.
#[tauri::command(async)]
fn triage_groups(state: State<AppState>, kind: String, variant: Option<String>, offset: i64, limit: i64) -> CmdResult<Vec<media_core::triage::TriageGroup>> {
    let conn = state.db.conn().map_err(err)?;
    media_core::triage::groups(&conn, &kind, variant.as_deref(), offset, limit).map_err(err)
}

/// Remember review decisions (kept files); removing the rest is a normal delete.
#[tauri::command(async)]
fn triage_commit(state: State<AppState>, decisions: Vec<media_core::triage::Decision>) -> CmdResult<()> {
    no_job(&state)?;
    let mut conn = state.db.conn().map_err(err)?;
    media_core::triage::commit(&mut conn, &decisions).map_err(err)?;
    state.changed();
    refresh_places(&state).map_err(err)
}

/// What to send when something went wrong: versions, the state of the database and of the
/// drives, and the last lines of the log. No file contents, no photo names beyond paths that
/// appear in error messages.
#[tauri::command(async)]
fn diagnostics(state: State<AppState>) -> CmdResult<String> {
    let mut out = vec![
        format!("Media Organizer v{} · {} {}", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH),
        format!("시각: {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S")),
        format!("데이터 폴더: {}", state.data_dir.display()),
        format!("진행 중인 작업: {}", state.job.locked().clone().unwrap_or_else(|| "없음".into())),
    ];
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let dbp = state.db.path();
    out.push(format!(
        "DB: {:.1} MB (+WAL {:.1} MB) · 저장된 복사본 {}개",
        size(dbp) as f64 / 1e6,
        size(&dbp.with_extension("db-wal")) as f64 / 1e6,
        state.db.snapshots().len()
    ));
    if let Ok(conn) = state.db.conn() {
        let n = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(-1);
        out.push(format!(
            "스키마 v{} · 파일 {} (없어짐 {}, 격리 {}) · 얼굴 {} · 인물 {}",
            db::schema_version(&conn).unwrap_or(-1),
            n("SELECT COUNT(*) FROM files"),
            n("SELECT COUNT(*) FROM files WHERE missing = 1"),
            n("SELECT COUNT(*) FROM files WHERE quarantined = 1"),
            n("SELECT COUNT(*) FROM faces"),
            n("SELECT COUNT(*) FROM persons"),
        ));
        let mut st = conn.prepare("SELECT state, COUNT(*) FROM batches GROUP BY 1 ORDER BY 1").map_err(err)?;
        let states: Vec<String> = st.query_map([], |r| Ok(format!("{} {}", r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).map_err(err)?.flatten().collect();
        out.push(format!("작업 기록: {} · 기록 {}줄 (대기 {})", states.join(", "), n("SELECT COUNT(*) FROM journal"), n("SELECT COUNT(*) FROM journal WHERE undone = 2")));
        let mut st = conn.prepare("SELECT label, online, removable, (SELECT COUNT(*) FROM files f WHERE f.volume_id = v.id) FROM volumes v ORDER BY id").map_err(err)?;
        for v in st.query_map([], |r| Ok(format!("드라이브 ‘{}’: {}{} · 파일 {}", r.get::<_, String>(0)?, if r.get::<_, i64>(1)? != 0 { "연결됨" } else { "연결 안 됨" }, if r.get::<_, i64>(2)? != 0 { " · 이동식" } else { "" }, r.get::<_, i64>(3)?))).map_err(err)?.flatten() {
            out.push(v);
        }
    }
    match state.db.quick_check() {
        Ok(p) if p.is_empty() => out.push("DB 검사: 이상 없음".into()),
        Ok(p) => out.push(format!("DB 검사: 문제 {}건 — {}", p.len(), p.join(" | "))),
        Err(e) => out.push(format!("DB 검사 실패: {e:#}")),
    }
    out.push(format!("캐시: {:.1} MB", state.thumbs.size_on_disk() as f64 / 1e6));
    out.push(String::new());
    out.push("── 최근 로그 ──".into());
    out.push(logs::tail(300));
    Ok(out.join("\n"))
}

/// Which of these files are protected from deletion, and why.
#[tauri::command(async)]
fn protect_check(state: State<AppState>, ids: Vec<i64>) -> CmdResult<Vec<media_core::protect::Blocked>> {
    let conn = state.db.conn().map_err(err)?;
    media_core::protect::check(&conn, &ids).map_err(err)
}

#[tauri::command(async)]
fn start_purge(app: AppHandle, state: State<AppState>, batch_id: String) -> CmdResult<()> {
    spawn_job(&app, &state, "purge", move |st, progress| {
        let conn = st.db.conn()?;
        journal::purge_batch_progress(&conn, &batch_id, &st.stop, progress)
    })
}

// ---------------------------------------------------------------------------------------------
// thumb:// protocol
// ---------------------------------------------------------------------------------------------

fn thumb_jpeg(bytes: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder().header("Content-Type", "image/jpeg").header("Cache-Control", "max-age=31536000, immutable").body(bytes).unwrap()
}

fn thumb_missing() -> Response<Vec<u8>> {
    Response::builder().status(StatusCode::NOT_FOUND).body(Vec::new()).unwrap()
}

/// The quick part of a thumbnail request: face crops and thumbnails made before are answered
/// at once; `Err` is a file whose thumbnail has to be made first.
fn thumb_lookup(app: &AppHandle, path: &str) -> Result<Response<Vec<u8>>, Box<db::FileRow>> {
    let key = path.trim_matches('/');
    let st = app.state::<AppState>();
    // Face crops: thumb://f-<face id> (cached at analysis time; copies reuse the original's).
    if let Some(face) = key.strip_prefix("f-").and_then(|s| s.parse::<i64>().ok()) {
        let src = st.db.conn().ok().and_then(|c| media_core::people::src_face(&c, face).ok()).flatten();
        return Ok(match st.thumbs.face_crop(face, src).and_then(|p| std::fs::read(p).ok()) {
            Some(bytes) => thumb_jpeg(bytes),
            None => thumb_missing(),
        });
    }
    let Some(id) = key.split('/').next().and_then(|s| s.parse::<i64>().ok()) else { return Ok(thumb_missing()) };
    let Ok(conn) = st.db.conn() else { return Ok(thumb_missing()) };
    let Ok(Some(f)) = db::get_file(&conn, id) else { return Ok(thumb_missing()) };
    match st.thumbs.cached_thumb(&f).and_then(|p| std::fs::read(p).ok()) {
        Some(bytes) => Ok(thumb_jpeg(bytes)),
        None => Err(Box::new(f)),
    }
}

/// The slow part: read the photo and make its thumbnail.
fn thumb_make(app: &AppHandle, f: &db::FileRow) -> Response<Vec<u8>> {
    match app.state::<AppState>().thumbs.ensure_thumb(f).and_then(|p| Ok(std::fs::read(p)?)) {
        Ok(bytes) => thumb_jpeg(bytes),
        Err(_) => thumb_missing(),
    }
}

/// The bundled ffmpeg/ffprobe live next to the executable; put that folder first on PATH so
/// both our code and czkawka_core find them without any system install.
fn prepend_exe_dir_to_path() {
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        let mut paths = vec![dir];
        if let Some(cur) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&cur));
        }
        if let Ok(joined) = std::env::join_paths(paths) {
            // SAFETY: called at startup before any other thread is spawned.
            unsafe { std::env::set_var("PATH", joined) };
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    prepend_exe_dir_to_path();
    let ran = tauri::Builder::default()
        // One app per PC: a second start shows the first window instead of opening the same
        // database and the same drives twice.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        // Closing the window while files are being moved or deleted: the job is asked to stop
        // (it does between two files), and the app closes when it has.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let st = window.state::<AppState>();
                if st.job.locked().is_some() {
                    api.prevent_close();
                    st.closing.store(true, std::sync::atomic::Ordering::SeqCst);
                    st.stop.stop();
                    let _ = window.emit("closing", ());
                }
            }
        })
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .register_asynchronous_uri_scheme_protocol("thumb", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            let path = request.uri().path().to_string();
            // Off the webview thread, and never more than a few at once: what is cached is
            // served by the quick lane; what has to be made waits its turn, newest first.
            let st = app.state::<AppState>();
            let app = app.clone();
            st.thumb_fast.push(move || match thumb_lookup(&app, &path) {
                Ok(response) => responder.respond(response),
                Err(file) => {
                    let again = app.clone();
                    app.state::<AppState>().thumb_make.push(move || responder.respond(thumb_make(&again, &file)));
                }
            });
        })
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let cache_dir = app.path().app_local_data_dir()?.join("cache");
            std::fs::create_dir_all(&data_dir)?;
            std::fs::create_dir_all(&cache_dir)?;
            logs::init(&data_dir.join("logs"));
            dedupe::init_czkawka(&cache_dir);
            let db = Db::open(data_dir.join("library.db"))?;
            let interrupted = {
                let conn = db.conn()?;
                volumes::refresh(&conn)?;
                // Keep the database tidy (cheap; helps the query planner on big libraries).
                let _ = conn.execute_batch("PRAGMA optimize;");
                journal::recover_interrupted(&conn).unwrap_or_default()
            };
            app.manage(AppState {
                interrupted: Mutex::new(interrupted),
                thumb_fast: media_core::WorkStack::new("thumb-look", 2),
                thumb_make: media_core::WorkStack::new("thumb-make", std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(2, 4)),
                data_gen: std::sync::atomic::AtomicU64::new(0),
                folders_cache: Default::default(),
                walk_ticket: std::sync::atomic::AtomicU64::new(0),
                closing: std::sync::atomic::AtomicBool::new(false),
                settings_lock: Mutex::new(()),
                db,
                thumbs: ThumbCache::new(&cache_dir),
                data_dir,
                cache_dir,
                stop: StopFlag::new(),
                job: Mutex::new(None),
                plans: Mutex::new(HashMap::new()),
                lane: Arc::new(Lane::default()),
            });
            // Resume background analysis a few seconds after start (let the window settle first).
            let handle = app.handle().clone();
            let st = app.state::<AppState>();
            st.lane.watch_power(&handle);
            watch_drives(&handle);
            let lane = st.lane.clone();
            std::thread::spawn(move || {
                // Groups saved before copies on other drives counted as backups: decide them again
                // once (nothing is read; later manual choices are not touched).
                if let Err(e) = upgrade_dup_keepers(&handle.state::<AppState>()) {
                    log::warn!(target: "app", "duplicate keepers upgrade failed: {e:#}");
                }
                // Libraries scanned before places existed already have GPS: name them now.
                if let Err(e) = refresh_places(&handle.state::<AppState>()) {
                    log::warn!(target: "app", "places refresh failed: {e:#}");
                }
                std::thread::sleep(std::time::Duration::from_secs(5));
                lane.kick(&handle, false);
                lane.kick_backfill(&handle);
                // Upkeep, behind everything else: is the database file sound, a copy of it once
                // a day, and the cache without what is no longer needed.
                std::thread::sleep(std::time::Duration::from_secs(20));
                let st = handle.state::<AppState>();
                match st.db.quick_check() {
                    Ok(p) if p.is_empty() => {}
                    Ok(p) => {
                        log::error!(target: "app", "database check found problems: {}", p.join(" | "));
                        let newest = st.db.snapshots().first().map(|p| p.display().to_string()).unwrap_or_else(|| "없음".into());
                        let _ = handle.emit("notice", format!("데이터베이스 파일에 손상이 발견되었습니다({}건). 파일을 옮기거나 지우는 작업은 잠시 미루고, 「설정」의 「진단 정보 복사」 내용을 보내 주세요. 가장 최근에 저장된 복사본: {newest}", p.len()));
                    }
                    Err(e) => log::warn!(target: "app", "database check failed: {e:#}"),
                }
                if st.job.locked().is_none() {
                    snapshot_if_due(&st, "daily", std::time::Duration::from_secs(24 * 3600));
                }
                if let Ok(conn) = st.db.conn() {
                    match media_core::guarded("캐시 정리", || st.thumbs.prune(&conn, 20 << 30)) {
                        Ok(r) if r.removed > 0 => log::info!(target: "app", "cache: removed {} files, {:.1} MB", r.removed, r.bytes as f64 / 1e6),
                        Ok(_) => {}
                        Err(e) => log::warn!(target: "app", "cache upkeep failed: {e:#}"),
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            list_volumes,
            list_sources,
            add_source,
            remove_source,
            start_scan,
            cleanup_summary,
            trips_summary,
            compare_files,
            get_dup_rules,
            set_original,
            set_dup_rules,
            dup_rule_suggestions,
            similar_to,
            prepare_copies,
            people_list,
            person_faces,
            people_suggestions,
            rename_person,
            merge_people,
            reject_faces,
            protect_check,
            plan_tree,
            get_library_rules,
            set_library_rules,
            list_cards,
            get_import_settings,
            set_import_settings,
            start_import_preview,
            start_import,
            import_cancel,
            triage_groups,
            triage_commit,
            diagnostics,
            start_damage_check,
            move_faces,
            hide_people,
            set_person_cover,
            answer_people_pair,
            clear_face_data,
            burst_extra_ids,
            dismiss,
            undismiss,
            analysis_status,
            analysis_pause,
            analysis_start,
            get_analysis_options,
            set_analysis_options,
            get_scan_options,
            set_scan_options,
            default_scan_options,
            suggested_folders,
            filter_options,
            filtered_stats,
            group_summary,
            start_thumbs,
            cancel_job,
            library_stats,
            timeline,
            grid,
            query_ids,
            file_detail,
            preview_path,
            video_preview_path,
            reveal_file,
            open_file,
            vlc_path,
            open_in_vlc,
            check_update,
            install_update,
            label_counts,
            add_label,
            remove_label,
            set_date,
            shift_date,
            start_dedupe,
            dup_summary,
            dup_groups,
            set_keep,
            start_dedupe_apply,
            start_plan,
            plan_items,
            start_execute,
            batches,
            start_undo,
            start_delete,
            start_purge,
            library_years,
            year_copies,
            year_copy_pairs,
            year_keep,
            disk_space,
            set_faces_enabled,
            file_family,
            group_items,
            year_triage_groups,
            year_triage_counts,
            get_library_merges,
            set_library_merges,
            library_excluded_count,
            clear_library_excluded,
            library_delete_preview,
            start_library_delete,
            copy_folders,
            folder_files,
            folder_compare,
            folder_props,
            folder_align,
            folder_first_difference,
            cancel_folder_check,
            start_folder_swap,
            folders_reviewed,
            set_folder_reviewed,
            start_remove_copy_folders,
            open_folder,
        ])
        .run(tauri::generate_context!());
    // Not a panic into nothing: the reason is written down and, on Windows, shown.
    if let Err(e) = ran {
        logs::start_failed(&format!("{e:#}"));
        std::process::exit(1);
    }
}

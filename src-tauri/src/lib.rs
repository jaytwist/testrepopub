//! Tauri shell: a thin command layer over `media-core`.
//!
//! * Long jobs (scan, thumbnails, dedupe, plan, organize, undo) run one at a time on a worker
//!   thread and report through `job-progress` / `job-done` events.
//! * Thumbnails are served through the `thumb://` protocol, generated on demand and cached.
//! * Full-size previews use Tauri's asset protocol (`convertFileSrc`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use media_core::db::{self, FileFilter, FileRow, FilterOptions, FilteredStats, GridItem, LibraryStats, MonthCount, SourceRow, VolumeRow};
use media_core::dedupe::{self, DupGroup, DupKind, DupSummary, Similarity};
use media_core::journal::{self, BatchRow};
use media_core::labels::{self, LabelCount, LabelRow};
use media_core::organizer::{self, OrganizeOptions, Plan, PlanItem, PlanSummary};
use media_core::scanner;
use media_core::userfiles::{self, ScanOptions};
use media_core::thumbs::{self, ThumbCache};
use media_core::{meta, volumes, Db, Progress, StopFlag};
use serde::Serialize;
use tauri::http::{Response, StatusCode};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;

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
}

#[derive(Clone, Serialize)]
struct JobDone {
    job: String,
    ok: bool,
    error: Option<String>,
    result: serde_json::Value,
}

/// Run `f` on a worker thread as the single active job.
fn spawn_job<F, R>(app: &AppHandle, state: &AppState, name: &str, f: F) -> CmdResult<()>
where
    F: FnOnce(&AppState, &(dyn Fn(Progress) + Sync)) -> anyhow::Result<R> + Send + 'static,
    R: Serialize,
{
    {
        let mut j = state.job.lock().unwrap();
        if let Some(cur) = j.as_ref() {
            return Err(format!("다른 작업이 진행 중입니다: {cur}"));
        }
        *j = Some(name.to_string());
    }
    state.stop.reset();
    let app = app.clone();
    let name = name.to_string();
    std::thread::spawn(move || {
        let st = app.state::<AppState>();
        let emit_app = app.clone();
        let last = Mutex::new(std::time::Instant::now() - std::time::Duration::from_secs(1));
        let progress = move |p: Progress| {
            // Throttle UI updates to ~15/s, but always deliver stage changes and completion.
            let mut l = last.lock().unwrap();
            if l.elapsed().as_millis() >= 66 || p.done == p.total || p.done == 0 {
                *l = std::time::Instant::now();
                let _ = emit_app.emit("job-progress", p);
            }
        };
        let res = f(&st, &progress);
        *st.job.lock().unwrap() = None;
        let done = match res {
            Ok(r) => JobDone { job: name, ok: true, error: None, result: serde_json::to_value(r).unwrap_or_default() },
            Err(e) => JobDone { job: name, ok: false, error: Some(format!("{e:#}")), result: serde_json::Value::Null },
        };
        let _ = app.emit("job-done", done);
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
}

#[tauri::command]
fn app_info(state: State<AppState>) -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        data_dir: state.data_dir.to_string_lossy().into(),
        cache_dir: state.cache_dir.to_string_lossy().into(),
        ffmpeg: dedupe::ffmpeg_available(),
        job: state.job.lock().unwrap().clone(),
    }
}

#[tauri::command]
fn list_volumes(state: State<AppState>, refresh: bool) -> CmdResult<Vec<VolumeRow>> {
    let conn = state.db.conn().map_err(err)?;
    if refresh {
        volumes::refresh(&conn).map_err(err)?;
    }
    db::list_volumes(&conn).map_err(err)
}

#[tauri::command]
fn list_sources(state: State<AppState>) -> CmdResult<Vec<SourceRow>> {
    let conn = state.db.conn().map_err(err)?;
    db::list_sources(&conn).map_err(err)
}

#[tauri::command]
fn add_source(state: State<AppState>, path: String) -> CmdResult<i64> {
    let conn = state.db.conn().map_err(err)?;
    volumes::add_source(&conn, Path::new(&path)).map_err(err)
}

/// Stop tracking a folder. Index rows for files under it are removed; files on disk are untouched.
#[tauri::command]
fn remove_source(state: State<AppState>, id: i64) -> CmdResult<()> {
    let conn = state.db.conn().map_err(err)?;
    volumes::remove_source(&conn, id).map_err(err)
}

#[tauri::command]
fn start_scan(app: AppHandle, state: State<AppState>, source_id: Option<i64>) -> CmdResult<()> {
    spawn_job(&app, &state, "scan", move |st, progress| {
        let opts = userfiles::load_options(&st.db.conn()?)?;
        let stats = match source_id {
            Some(id) => vec![scanner::scan_source(&st.db, id, &opts, &st.stop, progress)?],
            None => scanner::scan_all(&st.db, &opts, &st.stop, progress)?,
        };
        // Thumbnails right away, so the drive can be unplugged and still browsed.
        let warm = thumbs::warm(&st.db, &st.thumbs, &st.stop, progress)?;
        Ok(serde_json::json!({ "scan": stats, "thumbs": warm }))
    })
}

#[tauri::command]
fn get_scan_options(state: State<AppState>) -> CmdResult<ScanOptions> {
    let conn = state.db.conn().map_err(err)?;
    userfiles::load_options(&conn).map_err(err)
}

#[tauri::command]
fn set_scan_options(state: State<AppState>, options: ScanOptions) -> CmdResult<()> {
    let conn = state.db.conn().map_err(err)?;
    userfiles::save_options(&conn, &options).map_err(err)
}

#[tauri::command]
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
#[tauri::command]
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

#[tauri::command]
fn start_thumbs(app: AppHandle, state: State<AppState>) -> CmdResult<()> {
    spawn_job(&app, &state, "thumbs", |st, progress| thumbs::warm(&st.db, &st.thumbs, &st.stop, progress))
}

#[tauri::command]
fn cancel_job(state: State<AppState>) {
    state.stop.stop();
}

// ---------------------------------------------------------------------------------------------
// Library browsing
// ---------------------------------------------------------------------------------------------

#[tauri::command]
fn library_stats(state: State<AppState>) -> CmdResult<LibraryStats> {
    let conn = state.db.conn().map_err(err)?;
    db::library_stats(&conn).map_err(err)
}

#[tauri::command]
fn timeline(state: State<AppState>, filter: FileFilter) -> CmdResult<Vec<MonthCount>> {
    let conn = state.db.conn().map_err(err)?;
    db::timeline_summary(&conn, &filter).map_err(err)
}

#[tauri::command]
fn filter_options(state: State<AppState>) -> CmdResult<FilterOptions> {
    let conn = state.db.conn().map_err(err)?;
    db::filter_options(&conn).map_err(err)
}

#[tauri::command]
fn filtered_stats(state: State<AppState>, filter: FileFilter) -> CmdResult<FilteredStats> {
    let conn = state.db.conn().map_err(err)?;
    db::filtered_stats(&conn, &filter).map_err(err)
}

#[tauri::command]
fn grid(state: State<AppState>, filter: FileFilter, offset: i64, limit: i64) -> CmdResult<Vec<GridItem>> {
    let conn = state.db.conn().map_err(err)?;
    db::query_grid(&conn, &filter, offset, limit.min(5000)).map_err(err)
}

#[tauri::command]
fn query_ids(state: State<AppState>, filter: FileFilter) -> CmdResult<Vec<i64>> {
    let conn = state.db.conn().map_err(err)?;
    db::query_ids(&conn, &filter).map_err(err)
}

#[derive(Serialize)]
struct FileDetail {
    file: FileRow,
    labels: Vec<LabelRow>,
    abs_path: String,
    taken: String,
    confidence: String,
    thumb_cached: bool,
}

#[tauri::command]
fn file_detail(state: State<AppState>, id: i64) -> CmdResult<FileDetail> {
    let conn = state.db.conn().map_err(err)?;
    let f = db::get_file(&conn, id).map_err(err)?.ok_or("not found")?;
    Ok(FileDetail {
        labels: labels::labels_for(&conn, id).map_err(err)?,
        abs_path: f.abs_path().to_string_lossy().into(),
        taken: f.taken_at.map(meta::fmt_ts).unwrap_or_default(),
        confidence: meta::confidence(&f.date_source).into(),
        thumb_cached: state.thumbs.has_thumb(&f),
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

#[tauri::command]
fn reveal_file(app: AppHandle, state: State<AppState>, id: i64) -> CmdResult<()> {
    let conn = state.db.conn().map_err(err)?;
    let f = db::get_file(&conn, id).map_err(err)?.ok_or("not found")?;
    app.opener().reveal_item_in_dir(f.abs_path()).map_err(err)
}

#[tauri::command]
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

#[tauri::command]
fn vlc_path() -> Option<String> {
    find_vlc().map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
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
        let mut j = st.job.lock().unwrap();
        if let Some(cur) = j.as_ref() {
            return Err(format!("작업이 끝난 뒤 업데이트할 수 있습니다 (진행 중: {cur})"));
        }
        *j = Some("update".into());
    }
    let release = |app: &AppHandle| *app.state::<AppState>().job.lock().unwrap() = None;
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

#[tauri::command]
fn label_counts(state: State<AppState>) -> CmdResult<Vec<LabelCount>> {
    let conn = state.db.conn().map_err(err)?;
    labels::label_counts(&conn).map_err(err)
}

#[tauri::command]
fn add_label(state: State<AppState>, ids: Vec<i64>, label: String) -> CmdResult<usize> {
    let mut conn = state.db.conn().map_err(err)?;
    labels::add_user_label(&mut conn, &ids, &label).map_err(err)
}

#[tauri::command]
fn remove_label(state: State<AppState>, ids: Vec<i64>, label: String) -> CmdResult<usize> {
    let mut conn = state.db.conn().map_err(err)?;
    labels::remove_label(&mut conn, &ids, &label).map_err(err)
}

/// `datetime` is local wall time, "YYYY-MM-DDTHH:MM" (from an <input type="datetime-local">).
#[tauri::command]
fn set_date(state: State<AppState>, ids: Vec<i64>, datetime: String) -> CmdResult<usize> {
    let dt = chrono::NaiveDateTime::parse_from_str(&datetime, "%Y-%m-%dT%H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(&datetime, "%Y-%m-%dT%H:%M:%S"))
        .map_err(err)?;
    let mut conn = state.db.conn().map_err(err)?;
    labels::set_taken_at(&mut conn, &ids, meta::to_ts(dt)).map_err(err)
}

#[tauri::command]
fn shift_date(state: State<AppState>, ids: Vec<i64>, delta_secs: i64) -> CmdResult<usize> {
    let mut conn = state.db.conn().map_err(err)?;
    labels::shift_taken_at(&mut conn, &ids, delta_secs).map_err(err)
}

// ---------------------------------------------------------------------------------------------
// Duplicates
// ---------------------------------------------------------------------------------------------

#[tauri::command]
fn start_dedupe(app: AppHandle, state: State<AppState>, kind: DupKind, similarity: Option<Similarity>) -> CmdResult<()> {
    spawn_job(&app, &state, &format!("dedupe:{}", kind.as_str()), move |st, progress| {
        dedupe::run(&st.db, kind, similarity.unwrap_or(Similarity::High), &st.stop, progress)
    })
}

#[tauri::command]
fn dup_summary(state: State<AppState>) -> CmdResult<Vec<DupSummary>> {
    let conn = state.db.conn().map_err(err)?;
    dedupe::summary(&conn).map_err(err)
}

#[tauri::command]
fn dup_groups(state: State<AppState>, kind: String, offset: i64, limit: i64) -> CmdResult<Vec<DupGroup>> {
    let conn = state.db.conn().map_err(err)?;
    dedupe::list_groups(&conn, &kind, offset, limit).map_err(err)
}

#[tauri::command]
fn set_keep(state: State<AppState>, group_id: i64, file_id: i64, keep: bool) -> CmdResult<()> {
    let conn = state.db.conn().map_err(err)?;
    dedupe::set_keep(&conn, group_id, file_id, keep).map_err(err)
}

#[tauri::command]
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

#[tauri::command]
fn start_plan(app: AppHandle, state: State<AppState>, options: OrganizeOptions) -> CmdResult<()> {
    spawn_job(&app, &state, "organize:plan", move |st, progress| {
        let plan = organizer::plan(&st.db, &options, &st.stop, progress)?;
        let view = PlanView { id: plan.id.clone(), summary: plan.summary.clone(), options: plan.options.clone() };
        let mut plans = st.plans.lock().unwrap();
        plans.clear();
        plans.insert(plan.id.clone(), Arc::new(plan));
        Ok(view)
    })
}

#[tauri::command]
fn plan_items(state: State<AppState>, plan_id: String, action: Option<String>, offset: usize, limit: usize) -> CmdResult<Vec<PlanItem>> {
    let plans = state.plans.lock().unwrap();
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

#[tauri::command]
fn start_execute(app: AppHandle, state: State<AppState>, plan_id: String) -> CmdResult<()> {
    let plan = state.plans.lock().unwrap().get(&plan_id).cloned().ok_or("계획이 만료되었습니다. 다시 미리보기를 실행하세요")?;
    spawn_job(&app, &state, "organize:run", move |st, progress| {
        let r = organizer::execute(&st.db, &plan, &st.stop, progress)?;
        st.plans.lock().unwrap().remove(&plan.id);
        Ok(r)
    })
}

// ---------------------------------------------------------------------------------------------
// History (undo / purge)
// ---------------------------------------------------------------------------------------------

#[tauri::command]
fn batches(state: State<AppState>) -> CmdResult<Vec<BatchRow>> {
    let conn = state.db.conn().map_err(err)?;
    journal::list_batches(&conn).map_err(err)
}

#[tauri::command]
fn start_undo(app: AppHandle, state: State<AppState>, batch_id: String) -> CmdResult<()> {
    spawn_job(&app, &state, "undo", move |st, _| {
        let conn = st.db.conn()?;
        volumes::refresh(&conn)?;
        journal::undo_batch(&conn, &batch_id)
    })
}

#[tauri::command]
fn start_purge(app: AppHandle, state: State<AppState>, batch_id: String) -> CmdResult<()> {
    spawn_job(&app, &state, "purge", move |st, _| {
        let conn = st.db.conn()?;
        journal::purge_batch(&conn, &batch_id)
    })
}

// ---------------------------------------------------------------------------------------------
// thumb:// protocol
// ---------------------------------------------------------------------------------------------

fn thumb_response(app: &AppHandle, path: &str) -> Response<Vec<u8>> {
    let not_found = || Response::builder().status(StatusCode::NOT_FOUND).body(Vec::new()).unwrap();
    let Some(id) = path.trim_matches('/').split('/').next().and_then(|s| s.parse::<i64>().ok()) else { return not_found() };
    let st = app.state::<AppState>();
    let Ok(conn) = st.db.conn() else { return not_found() };
    let Ok(Some(f)) = db::get_file(&conn, id) else { return not_found() };
    match st.thumbs.ensure_thumb(&f).and_then(|p| Ok(std::fs::read(p)?)) {
        Ok(bytes) => Response::builder()
            .header("Content-Type", "image/jpeg")
            .header("Cache-Control", "max-age=31536000, immutable")
            .body(bytes)
            .unwrap(),
        Err(_) => not_found(),
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
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .register_asynchronous_uri_scheme_protocol("thumb", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            let path = request.uri().path().to_string();
            // Thumbnail generation can take ~100 ms; keep it off the webview thread.
            std::thread::spawn(move || responder.respond(thumb_response(&app, &path)));
        })
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let cache_dir = app.path().app_local_data_dir()?.join("cache");
            std::fs::create_dir_all(&data_dir)?;
            std::fs::create_dir_all(&cache_dir)?;
            dedupe::init_czkawka(&cache_dir);
            let db = Db::open(data_dir.join("library.db"))?;
            {
                let conn = db.conn()?;
                volumes::refresh(&conn)?;
            }
            app.manage(AppState {
                db,
                thumbs: ThumbCache::new(&cache_dir),
                data_dir,
                cache_dir,
                stop: StopFlag::new(),
                job: Mutex::new(None),
                plans: Mutex::new(HashMap::new()),
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
            get_scan_options,
            set_scan_options,
            default_scan_options,
            suggested_folders,
            filter_options,
            filtered_stats,
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
            start_purge,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Media Organizer");
}

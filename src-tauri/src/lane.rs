//! Background analysis lane: runs `media_core::analysis` beside (not instead of) user jobs.
//! It pauses itself while a user job (scan, delete, organize, undo...) runs, when the user presses
//! pause, or on laptop battery; it resumes on its own afterwards.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use media_core::analysis::{self, AnalysisOptions, FaceRuntime, PauseFlag};
use media_core::faces::FaceEngine;
use media_core::{power, Progress, StopFlag};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;

#[derive(Clone, Debug, Default, Serialize)]
pub struct LaneStatus {
    pub running: bool,
    pub paused: bool,
    /// "user" | "job" | "battery"
    pub reason: Option<String>,
    pub done: u64,
    pub total: u64,
    pub eta_secs: u64,
    pub current: String,
    /// "CPU" | "DirectML" | "-" (no face analysis)
    pub provider: String,
    pub pending_online: i64,
    pub pending_offline: i64,
    pub analyzed: i64,
    pub faces_available: bool,
    /// Faces found so far (diagnostics on the People screen).
    pub faces_found: i64,
    pub last_error: Option<String>,
}

#[derive(Default)]
pub struct Lane {
    running: AtomicBool,
    user_paused: AtomicBool,
    job_active: AtomicBool,
    on_battery: AtomicBool,
    pub pause: PauseFlag,
    pub stop: StopFlag,
    status: Mutex<LaneStatus>,
    engine: Mutex<Option<Arc<Mutex<FaceEngine>>>>,
}

/// How often people / bursts / blur are regrouped during a long analysis pass (env
/// `MO_REGROUP_SECS` overrides, for testing).
fn regroup_every() -> Duration {
    Duration::from_secs(std::env::var("MO_REGROUP_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(120))
}

/// Where the ONNX Runtime library and models are: `MO_AI_DIR` (dev), the installer's resources,
/// or an `ai` folder next to the exe (portable zip) — the first that has everything.
pub fn face_runtime(app: &AppHandle) -> FaceRuntime {
    let lib = if cfg!(windows) { "onnxruntime.dll" } else if cfg!(target_os = "macos") { "libonnxruntime.dylib" } else { "libonnxruntime.so" };
    let dirs = [
        std::env::var_os("MO_AI_DIR").map(PathBuf::from),
        app.path().resource_dir().ok().map(|d| d.join("ai")),
        std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("ai"))),
    ];
    let mut first = None;
    for dir in dirs.into_iter().flatten() {
        let rt = FaceRuntime { lib: dir.join(lib), models: dir };
        if rt.available() {
            return rt;
        }
        first.get_or_insert(rt);
    }
    first.unwrap_or(FaceRuntime { lib: PathBuf::from(lib), models: PathBuf::new() })
}

/// Face analysis actually happens only when it is on and the AI files are present; otherwise
/// face work must not count as pending (the lane would wait for it forever).
fn people_active(app: &AppHandle, opts: &AnalysisOptions) -> bool {
    opts.people && face_runtime(app).available()
}

impl Lane {
    fn options(app: &AppHandle) -> AnalysisOptions {
        let st = app.state::<AppState>();
        st.db.conn().ok().and_then(|c| analysis::load_options(&c).ok()).unwrap_or_default()
    }

    fn update_pause(&self, app: &AppHandle) {
        let opts = Self::options(app);
        let reason = if self.user_paused.load(Ordering::Relaxed) {
            Some("user")
        } else if self.job_active.load(Ordering::Relaxed) {
            Some("job")
        } else if opts.pause_on_battery && self.on_battery.load(Ordering::Relaxed) {
            Some("battery")
        } else {
            None
        };
        self.pause.set(reason.is_some());
        let mut s = self.status.lock().unwrap();
        s.paused = reason.is_some();
        s.reason = reason.map(String::from);
        let snapshot = s.clone();
        drop(s);
        let _ = app.emit("analysis-status", snapshot);
    }

    pub fn set_user_paused(&self, app: &AppHandle, paused: bool) {
        self.user_paused.store(paused, Ordering::Relaxed);
        self.update_pause(app);
    }

    /// Called around every user job.
    pub fn set_job_active(&self, app: &AppHandle, active: bool) {
        self.job_active.store(active, Ordering::Relaxed);
        self.update_pause(app);
    }

    pub fn status(&self, app: &AppHandle) -> LaneStatus {
        let st = app.state::<AppState>();
        let opts = Self::options(app);
        let mut s = self.status.lock().unwrap().clone();
        if let Ok(conn) = st.db.conn() {
            if let Ok(p) = analysis::pending(&conn, people_active(app, &opts)) {
                s.pending_online = p.online;
                s.pending_offline = p.offline;
                s.analyzed = p.analyzed;
            }
        }
        s.running = self.running.load(Ordering::Relaxed);
        s.faces_available = face_runtime(app).available();
        if let Ok(conn) = st.db.conn() {
            s.faces_found = conn.query_row("SELECT COUNT(*) FROM faces", [], |r| r.get(0)).unwrap_or(0);
        }
        s
    }

    /// Drop the cached face engine (people feature toggled or GPU setting changed).
    pub fn reset_engine(&self) {
        *self.engine.lock().unwrap() = None;
    }

    fn engine(&self, app: &AppHandle, opts: &AnalysisOptions) -> Result<Option<Arc<Mutex<FaceEngine>>>, String> {
        if !opts.people {
            return Ok(None);
        }
        let mut cached = self.engine.lock().unwrap();
        if let Some(e) = cached.as_ref() {
            return Ok(Some(e.clone()));
        }
        let rt = face_runtime(app);
        if !rt.available() {
            return Err("인물 기능에 필요한 파일(ONNX Runtime·모델)이 설치되어 있지 않습니다".into());
        }
        let e = FaceEngine::load(&rt.lib, &rt.models, opts.gpu, 2).map_err(|e| format!("{e:#}"))?;
        let e = Arc::new(Mutex::new(e));
        *cached = Some(e.clone());
        Ok(Some(e))
    }

    /// Start analysing if there is work and nothing is running. `force` ignores the auto setting.
    pub fn kick(self: &Arc<Self>, app: &AppHandle, force: bool) {
        let opts = Self::options(app);
        if !opts.auto && !force {
            return;
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let lane = self.clone();
        let app = app.clone();
        std::thread::spawn(move || {
            lane.stop.reset();
            lane.update_pause(&app);
            let st = app.state::<AppState>();
            let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(1).max(1);
            loop {
                let opts = Self::options(&app);
                let engine = match lane.engine(&app, &opts) {
                    Ok(e) => {
                        lane.status.lock().unwrap().last_error = None;
                        e
                    }
                    Err(msg) => {
                        lane.status.lock().unwrap().last_error = Some(format!("인물(얼굴) 분석을 시작하지 못했습니다: {msg}"));
                        None
                    }
                };
                // People, bursts and blur are regrouped every couple of minutes while a long pass
                // runs, so the People screen fills in progressively instead of after hours.
                let ticking = Arc::new(AtomicBool::new(true));
                {
                    let ticking = ticking.clone();
                    let app = app.clone();
                    std::thread::spawn(move || {
                        let mut waited = Duration::ZERO;
                        while ticking.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_secs(1));
                            waited += Duration::from_secs(1);
                            if waited >= regroup_every() {
                                waited = Duration::ZERO;
                                let st = app.state::<AppState>();
                                if crate::post_analysis(&st).is_ok() {
                                    let _ = app.emit("analysis-done", ());
                                }
                            }
                        }
                    });
                }
                let emit_app = app.clone();
                let status_lane = lane.clone();
                let last = Mutex::new(Instant::now() - Duration::from_secs(1));
                let progress = move |p: Progress| {
                    let mut s = status_lane.status.lock().unwrap();
                    s.done = p.done;
                    s.total = p.total;
                    if let Some((name, eta)) = p.message.rsplit_once('|') {
                        s.current = name.to_string();
                        s.eta_secs = eta.parse().unwrap_or(0);
                    }
                    let mut l = last.lock().unwrap();
                    if l.elapsed().as_millis() >= 250 || p.done == p.total {
                        *l = Instant::now();
                        let _ = emit_app.emit("analysis-status", s.clone());
                    }
                };
                {
                    let mut s = lane.status.lock().unwrap();
                    s.running = true;
                    s.provider = engine.as_ref().and_then(|e| e.lock().ok().map(|e| e.provider.clone())).unwrap_or_else(|| "-".into());
                }
                let res = analysis::run(&st.db, &st.thumbs, engine.as_deref(), threads, &lane.stop, &lane.pause, &progress);
                ticking.store(false, Ordering::Relaxed);
                if let Err(e) = &res {
                    lane.status.lock().unwrap().last_error = Some(format!("{e:#}"));
                }
                // Derived data (bursts, places, trips, people) is recomputed after each pass.
                if let Err(e) = crate::post_analysis(&st) {
                    lane.status.lock().unwrap().last_error = Some(format!("{e:#}"));
                }
                let again = !lane.stop.is_stopped()
                    && res.is_ok()
                    && st.db.conn().ok().and_then(|c| analysis::pending(&c, engine.is_some()).ok()).map(|p| p.online > 0).unwrap_or(false);
                if !again {
                    break;
                }
            }
            lane.running.store(false, Ordering::SeqCst);
            let snapshot = {
                let mut s = lane.status.lock().unwrap();
                s.running = false;
                s.clone()
            };
            let _ = app.emit("analysis-status", snapshot);
            let _ = app.emit("analysis-done", ());
        });
    }

    /// Poll the power source; pause/resume on battery changes.
    pub fn watch_power(self: &Arc<Self>, app: &AppHandle) {
        let lane = self.clone();
        let app = app.clone();
        std::thread::spawn(move || loop {
            let b = power::on_battery();
            if lane.on_battery.swap(b, Ordering::Relaxed) != b {
                lane.update_pause(&app);
            }
            std::thread::sleep(Duration::from_secs(30));
        });
    }
}

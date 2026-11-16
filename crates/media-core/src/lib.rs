//! media-core: everything the desktop app does, without any UI or webview dependency.
//!
//! The Tauri shell (`src-tauri`) is a thin command layer over this crate, so all of the
//! scanning, labeling, de-duplication and organizing logic can be tested headlessly.

/// Re-exported so the app can pass connections around without its own dependency.
pub use rusqlite;
pub mod db;
pub mod analysis;
pub mod cleanup;
pub mod compare;
pub mod dedupe;
pub mod faces;
pub mod folders;
pub mod identity;
pub mod importer;
pub mod journal;
pub mod library;
pub mod labels;
pub mod media;
pub mod meta;
pub mod organizer;
pub mod people;
pub mod places;
pub mod power;
pub mod protect;
pub mod scanner;
pub mod thumbs;
pub mod triage;
pub mod userfiles;
pub mod volumes;

pub use db::Db;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Cooperative cancellation flag shared between a long-running job and the UI.
#[derive(Clone, Default)]
pub struct StopFlag(Arc<AtomicBool>);

impl StopFlag {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn reset(&self) {
        self.0.store(false, Ordering::Relaxed);
    }
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    pub fn inner(&self) -> &Arc<AtomicBool> {
        &self.0
    }
}

/// Progress reported by long-running jobs (scan, hash, similar search, organize).
#[derive(Clone, Debug, serde::Serialize)]
pub struct Progress {
    pub job: String,
    pub stage: String,
    pub done: u64,
    pub total: u64,
    pub message: String,
    /// For work measured in bytes (reading files): how far it is, how fast it goes (bytes per
    /// second) and how long it has left, once that can be told.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_done: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<u64>,
}

impl Progress {
    pub fn new(job: &str, stage: &str, done: u64, total: u64, message: impl Into<String>) -> Self {
        Self { job: job.into(), stage: stage.into(), done, total, message: message.into(), bytes_done: None, bytes_total: None, rate: None, eta_secs: None }
    }

    /// Add the bytes read so far of the bytes to read, and the time the work has taken: speed
    /// and time left follow (not in the first seconds, when they would be guesses).
    pub fn with_bytes(mut self, done: u64, total: u64, elapsed: std::time::Duration) -> Self {
        self.bytes_done = Some(done);
        self.bytes_total = Some(total);
        let secs = elapsed.as_secs_f64();
        if secs >= 10.0 && done > 0 {
            let rate = done as f64 / secs;
            self.rate = Some(rate as u64);
            self.eta_secs = Some((total.saturating_sub(done) as f64 / rate).ceil() as u64);
        }
        self
    }
}

pub type ProgressFn<'a> = &'a (dyn Fn(Progress) + Sync);

type Job = Box<dyn FnOnce() + Send>;
type JobStack = (std::sync::Mutex<(Vec<Job>, bool)>, std::sync::Condvar);

/// A fixed number of worker threads that always take the newest job first.
///
/// For work asked for by what is on screen (thumbnails): however fast the user moves, only so
/// many run at once — a hundred photos decoded at the same moment stall the disk and the whole
/// PC — and what was asked for last, what is on screen now, is done first. Nothing is dropped:
/// older jobs are done once the newer ones are.
pub struct WorkStack {
    shared: Arc<JobStack>,
}

impl WorkStack {
    pub fn new(name: &str, workers: usize) -> Self {
        let shared: Arc<JobStack> = Arc::new((std::sync::Mutex::new((Vec::new(), false)), std::sync::Condvar::new()));
        for i in 0..workers.max(1) {
            let shared = shared.clone();
            let _ = std::thread::Builder::new().name(format!("{name}-{i}")).spawn(move || loop {
                let job = {
                    let mut g = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                    loop {
                        if let Some(j) = g.0.pop() {
                            break j;
                        }
                        if g.1 {
                            return;
                        }
                        g = shared.1.wait(g).unwrap_or_else(|e| e.into_inner());
                    }
                };
                // One job that panics must not take its worker with it.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            });
        }
        Self { shared }
    }

    pub fn push(&self, job: impl FnOnce() + Send + 'static) {
        self.shared.0.lock().unwrap_or_else(|e| e.into_inner()).0.push(Box::new(job));
        self.shared.1.notify_one();
    }

    /// Jobs waiting (not counting those running).
    pub fn queued(&self) -> usize {
        self.shared.0.lock().unwrap_or_else(|e| e.into_inner()).0.len()
    }
}

impl Drop for WorkStack {
    fn drop(&mut self) {
        // The workers finish what is queued, then leave.
        self.shared.0.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
        self.shared.1.notify_all();
    }
}

/// Run `f`; a panic inside it (a decoder choking on a damaged file, say) becomes an error
/// instead of killing the thread — one bad file must not stop the rest.
pub fn guarded<T>(what: &str, f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => Err(anyhow::anyhow!("{what}: 예기치 못한 오류 ({})", panic_text(p.as_ref()))),
    }
}

/// What a panic said.
pub fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".into())
}

/// A lock that still opens after a thread died holding it (the data is plain state; a stuck
/// lock would freeze every later use).
pub trait Locked<T> {
    fn locked(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> Locked<T> for std::sync::Mutex<T> {
    fn locked(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

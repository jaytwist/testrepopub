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
}

impl Progress {
    pub fn new(job: &str, stage: &str, done: u64, total: u64, message: impl Into<String>) -> Self {
        Self { job: job.into(), stage: stage.into(), done, total, message: message.into() }
    }
}

pub type ProgressFn<'a> = &'a (dyn Fn(Progress) + Sync);

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

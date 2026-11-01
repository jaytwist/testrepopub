//! media-core: everything the desktop app does, without any UI or webview dependency.
//!
//! The Tauri shell (`src-tauri`) is a thin command layer over this crate, so all of the
//! scanning, labeling, de-duplication and organizing logic can be tested headlessly.

pub mod db;
pub mod dedupe;
pub mod journal;
pub mod labels;
pub mod media;
pub mod meta;
pub mod organizer;
pub mod scanner;
pub mod thumbs;
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

pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

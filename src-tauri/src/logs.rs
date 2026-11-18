//! A log file on the user's PC: what the app did and what went wrong, so a problem that shows
//! up there can be traced afterwards. One file per day in `<data>/logs`, the last two weeks
//! kept; warnings, errors and panics (with where they happened) — nothing about the photos
//! beyond the paths already in error messages.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

struct FileLog {
    dir: PathBuf,
    /// The open file and the day it is for.
    file: Mutex<Option<(String, File)>>,
}

static LOG: OnceLock<FileLog> = OnceLock::new();
const KEEP_DAYS: usize = 14;

impl FileLog {
    fn write(&self, line: &str) {
        let day = chrono::Local::now().format("%Y%m%d").to_string();
        let mut g = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if g.as_ref().map(|(d, _)| d != &day).unwrap_or(true) {
            let _ = std::fs::create_dir_all(&self.dir);
            *g = OpenOptions::new().create(true).append(true).open(self.dir.join(format!("app-{day}.log"))).ok().map(|f| (day, f));
            self.trim();
        }
        if let Some((_, f)) = g.as_mut() {
            let _ = writeln!(f, "{} {line}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"));
            let _ = f.flush();
        }
    }

    fn trim(&self) {
        let mut old: Vec<PathBuf> = std::fs::read_dir(&self.dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log")).collect();
        old.sort();
        for p in old.iter().rev().skip(KEEP_DAYS) {
            let _ = std::fs::remove_file(p);
        }
    }
}

impl log::Log for FileLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        // Our own crates down to `info`; libraries only when something is wrong.
        m.level() <= log::Level::Warn || (m.level() <= log::Level::Info && (m.target().starts_with("media_") || m.target().starts_with("app")))
    }

    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            self.write(&format!("{:5} {}: {}", r.level(), r.target(), r.args()));
        }
    }

    fn flush(&self) {}
}

/// Start logging into `dir`, and record panics there too (before the default handler runs).
pub fn init(dir: &Path) {
    let log = LOG.get_or_init(|| FileLog { dir: dir.to_path_buf(), file: Mutex::new(None) });
    if log::set_logger(log).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    let before = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let at = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let what = media_core::panic_text(info.payload());
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        if let Some(l) = LOG.get() {
            l.write(&format!("PANIC thread '{thread}' at {at}: {what}\n{}", std::backtrace::Backtrace::force_capture()));
        }
        before(info);
    }));
    log::info!(target: "app", "start v{} on {} {}", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH);
}

pub fn dir() -> Option<PathBuf> {
    LOG.get().map(|l| l.dir.clone())
}

/// The last `lines` lines logged (today's file, and yesterday's if today's is short).
pub fn tail(lines: usize) -> String {
    let Some(dir) = dir() else { return String::new() };
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log")).collect();
    files.sort();
    let mut out: Vec<String> = Vec::new();
    for f in files.iter().rev().take(2) {
        let text = std::fs::read_to_string(f).unwrap_or_default();
        let mut part: Vec<String> = text.lines().map(str::to_string).collect();
        part.extend(out);
        out = part;
        if out.len() >= lines {
            break;
        }
    }
    let from = out.len().saturating_sub(lines);
    out[from..].join("\n")
}

/// The app could not start: leave the reason where it can be found and, on Windows, say so (a
/// windowed program has no console to print to — without this the window just never appears).
pub fn start_failed(why: &str) {
    log::error!(target: "app", "start failed: {why}");
    let note = std::env::temp_dir().join("media-organizer-start-error.txt");
    let _ = std::fs::write(&note, format!("{}\n{why}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S")));
    #[cfg(windows)]
    {
        #[link(name = "user32")]
        extern "system" {
            fn MessageBoxW(hwnd: *mut std::ffi::c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
        }
        let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
        let text = wide(&format!(
            "Media Organizer를 시작하지 못했습니다.\n\n{why}\n\n· 창이 전혀 뜨지 않는다면 Microsoft Edge WebView2 런타임이 필요할 수 있습니다.\n· 자세한 내용: {}",
            note.display()
        ));
        let caption = wide("Media Organizer");
        // MB_OK | MB_ICONERROR
        unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), 0x10) };
    }
}

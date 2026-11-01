//! "Is this a user's photo/video, or something the system or an app put there?"
//!
//! Rules (all configurable through `ScanOptions`, persisted in the `settings` table):
//!  * System folders anywhere in the tree (Windows, Program Files, AppData, $Recycle.Bin, caches...)
//!  * Windows profile templates (Users\Default, Users\All Users)
//!  * Hidden folders/files (Hidden attribute or dot-names) and System-attribute files
//!  * App / game install folders: a folder with several .dll/.pak/.asar files or an uninstaller,
//!    plus everything below it (textures, icons, sample media shipped with programs)
//!  * OneDrive / cloud "online-only" files: reading them would download them
//!  * Icon-sized images: by bytes (fast, at walk time) and by pixels (after reading the header)
//!
//! Symlinks and NTFS junctions are never followed (walkdir `follow_links(false)`), which also avoids
//! the legacy "Application Data" / "My Documents" junction loops in Windows profiles.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::volumes;

/// Folder names that never contain the user's own media (matched case-insensitively, any depth).
pub const DEFAULT_EXCLUDED_DIRS: &[&str] = &[
    // Windows system
    "Windows", "Windows.old", "$Windows.~BT", "$Windows.~WS", "$WinREAgent", "$Recycle.Bin", "System Volume Information",
    "Recovery", "PerfLogs", "Config.Msi", "MSOCache", "ESD", "inetpub", "Intel", "AMD", "NVIDIA", "Drivers",
    // Programs and app data
    "Program Files", "Program Files (x86)", "ProgramData", "AppData", "WindowsApps", "OneDriveTemp",
    // Games
    "steamapps", "SteamLibrary", "Epic Games", "Riot Games", "XboxGames",
    // Developer folders and caches
    "node_modules", "site-packages", "__pycache__", "bower_components", ".git", ".svn", ".hg", ".idea", ".vscode",
    "cache", "caches", ".cache", "temp", "tmp", "thumbnails", ".thumbnails", ".trash", ".Trashes", ".Spotlight-V100",
    ".fseventsd", "lost+found",
    // This app's own folders
    volumes::QUARANTINE_DIR, ".media-organizer",
];

/// Children of a `Users` folder that are templates, not people.
const PROFILE_TEMPLATES: &[&str] = &["default", "default user", "all users", "defaultappool"];

/// Files that mark a folder as an installed program.
const APP_MARKER_EXTS: &[&str] = &["dll", "pak", "asar", "so", "dylib", "msi"];
const APP_MARKER_NAMES: &[&str] = &["unins000.exe", "uninstall.exe", "unins000.dat", "steam_api.dll", "steam_api64.dll"];
const APP_DIR_THRESHOLD: u32 = 3;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ScanOptions {
    /// Images smaller than this many bytes are skipped (videos never are).
    pub min_image_bytes: u64,
    /// Images whose longer side is below this many pixels are skipped (icons, stickers, web thumbnails).
    pub min_image_side: u32,
    pub skip_hidden: bool,
    pub skip_app_folders: bool,
    pub skip_cloud_only: bool,
    /// Folder names to skip (case-insensitive, any depth). Starts as `DEFAULT_EXCLUDED_DIRS`.
    pub exclude_dirs: Vec<String>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            min_image_bytes: 10 * 1024,
            min_image_side: 400,
            skip_hidden: true,
            skip_app_folders: true,
            skip_cloud_only: true,
            exclude_dirs: DEFAULT_EXCLUDED_DIRS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

pub fn load_options(conn: &Connection) -> Result<ScanOptions> {
    let v: Option<String> = conn.query_row("SELECT value FROM settings WHERE key = 'scan_options'", [], |r| r.get(0)).optional()?;
    Ok(v.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
}

pub fn save_options(conn: &Connection, opts: &ScanOptions) -> Result<()> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES ('scan_options', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![serde_json::to_string(opts)?],
    )?;
    Ok(())
}

/// Why something was not indexed. Counted per scan and shown to the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Skip {
    SystemFolder,
    HiddenFolder,
    AppFolder,
    HiddenFile,
    SystemFile,
    CloudOnly,
    TooSmall,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SkipCounts {
    pub system_folders: u64,
    pub hidden_folders: u64,
    pub app_files: u64,
    pub hidden_files: u64,
    pub system_files: u64,
    pub cloud_only: u64,
    pub too_small: u64,
}

impl SkipCounts {
    pub fn add(&mut self, s: Skip) {
        match s {
            Skip::SystemFolder => self.system_folders += 1,
            Skip::HiddenFolder => self.hidden_folders += 1,
            Skip::AppFolder => self.app_files += 1,
            Skip::HiddenFile => self.hidden_files += 1,
            Skip::SystemFile => self.system_files += 1,
            Skip::CloudOnly => self.cloud_only += 1,
            Skip::TooSmall => self.too_small += 1,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// File attributes
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
pub struct Attrs {
    pub hidden: bool,
    pub system: bool,
    pub cloud_only: bool,
}

#[cfg(windows)]
pub fn attrs(md: &std::fs::Metadata) -> Attrs {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN: u32 = 0x2;
    const SYSTEM: u32 = 0x4;
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_OPEN: u32 = 0x0004_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    let a = md.file_attributes();
    Attrs { hidden: a & HIDDEN != 0, system: a & SYSTEM != 0, cloud_only: a & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0 }
}

#[cfg(not(windows))]
pub fn attrs(_md: &std::fs::Metadata) -> Attrs {
    Attrs::default()
}

// ---------------------------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------------------------

pub struct Rules {
    excluded: HashSet<String>,
    opts: ScanOptions,
}

impl Rules {
    pub fn new(opts: &ScanOptions) -> Self {
        Self { excluded: opts.exclude_dirs.iter().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect(), opts: opts.clone() }
    }

    pub fn options(&self) -> &ScanOptions {
        &self.opts
    }

    /// Should the walker descend into this directory?
    pub fn dir_skip(&self, path: &Path, md: Option<&std::fs::Metadata>) -> Option<Skip> {
        let name = path.file_name().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        if self.excluded.contains(&name) {
            return Some(Skip::SystemFolder);
        }
        let parent = path.parent().and_then(|p| p.file_name()).map(|s| s.to_string_lossy().to_lowercase());
        if parent.as_deref() == Some("users") && PROFILE_TEMPLATES.contains(&name.as_str()) {
            return Some(Skip::SystemFolder);
        }
        let a = md.map(attrs).unwrap_or_default();
        if a.system {
            return Some(Skip::SystemFolder);
        }
        if self.opts.skip_hidden && (a.hidden || name.starts_with('.') || name.starts_with('$')) {
            return Some(Skip::HiddenFolder);
        }
        None
    }

    /// Cheap per-file check at walk time (before reading any content).
    pub fn file_skip(&self, name: &str, is_video: bool, size: u64, md: &std::fs::Metadata) -> Option<Skip> {
        let a = attrs(md);
        if a.system {
            return Some(Skip::SystemFile);
        }
        if self.opts.skip_hidden && (a.hidden || name.starts_with('.')) {
            return Some(Skip::HiddenFile);
        }
        if self.opts.skip_cloud_only && a.cloud_only {
            return Some(Skip::CloudOnly);
        }
        if !is_video && size < self.opts.min_image_bytes {
            return Some(Skip::TooSmall);
        }
        None
    }

    /// Check after the image header was read.
    pub fn pixels_too_small(&self, width: Option<i64>, height: Option<i64>) -> bool {
        match (width, height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => (w.max(h) as u32) < self.opts.min_image_side,
            _ => false,
        }
    }
}

/// Collects evidence of installed programs while walking, then answers "is this path inside one?".
#[derive(Default)]
pub struct AppDirDetector {
    markers: HashMap<PathBuf, u32>,
}

impl AppDirDetector {
    /// Feed every file name seen during the walk (not only media).
    pub fn observe(&mut self, path: &Path) {
        let Some(name) = path.file_name().map(|s| s.to_string_lossy().to_lowercase()) else { return };
        let ext = crate::media::ext_of(&name);
        let weight = if APP_MARKER_NAMES.contains(&name.as_str()) {
            APP_DIR_THRESHOLD
        } else if APP_MARKER_EXTS.contains(&ext.as_str()) {
            1
        } else {
            return;
        };
        if let Some(dir) = path.parent() {
            *self.markers.entry(dir.to_path_buf()).or_default() += weight;
        }
    }

    pub fn app_dirs(&self) -> BTreeSet<PathBuf> {
        self.markers.iter().filter(|(_, n)| **n >= APP_DIR_THRESHOLD).map(|(p, _)| p.clone()).collect()
    }

    /// True if `file` is inside an app dir found below `root` (the source folder itself is never one:
    /// if the user explicitly added a program folder, they meant it).
    pub fn contains(dirs: &BTreeSet<PathBuf>, root: &Path, file: &Path) -> bool {
        let mut cur = file.parent();
        while let Some(d) = cur {
            if d == root || !d.starts_with(root) {
                return false;
            }
            if dirs.contains(d) {
                return true;
            }
            cur = d.parent();
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirs() {
        let r = Rules::new(&ScanOptions::default());
        assert_eq!(r.dir_skip(Path::new("C:/Windows"), None), Some(Skip::SystemFolder));
        assert_eq!(r.dir_skip(Path::new("C:/Users/me/AppData"), None), Some(Skip::SystemFolder));
        assert_eq!(r.dir_skip(Path::new("C:/Users/Default"), None), Some(Skip::SystemFolder));
        assert_eq!(r.dir_skip(Path::new("D:/Photos/Default"), None), None, "only under Users");
        assert_eq!(r.dir_skip(Path::new("E:/DCIM/.thumbnails"), None), Some(Skip::SystemFolder));
        assert_eq!(r.dir_skip(Path::new("E:/backup/.hidden"), None), Some(Skip::HiddenFolder));
        assert_eq!(r.dir_skip(Path::new("C:/Users/me/Pictures"), None), None);
        assert_eq!(r.dir_skip(Path::new("D:/2019 제주도"), None), None);
        let mut o = ScanOptions::default();
        o.skip_hidden = false;
        o.exclude_dirs.retain(|d| d != "temp");
        let r = Rules::new(&o);
        assert_eq!(r.dir_skip(Path::new("E:/backup/.hidden"), None), None);
        assert_eq!(r.dir_skip(Path::new("E:/temp"), None), None, "user removed 'temp' from the list");
    }

    #[test]
    fn app_dirs() {
        let mut d = AppDirDetector::default();
        let root = Path::new("/d");
        for f in ["a.dll", "b.dll", "c.dll", "assets/icon.png"] {
            d.observe(&root.join("Games/MyGame").join(f));
        }
        d.observe(&root.join("Downloads/setup.dll"));
        d.observe(&root.join("Tool/unins000.exe"));
        let dirs = d.app_dirs();
        assert!(AppDirDetector::contains(&dirs, root, &root.join("Games/MyGame/assets/icon.png")));
        assert!(AppDirDetector::contains(&dirs, root, &root.join("Tool/logo.jpg")));
        assert!(!AppDirDetector::contains(&dirs, root, &root.join("Downloads/photo.jpg")), "one stray dll is not an app");
        assert!(!AppDirDetector::contains(&dirs, root, &root.join("Pictures/a.jpg")));
    }

    #[test]
    fn pixels() {
        let r = Rules::new(&ScanOptions::default());
        assert!(r.pixels_too_small(Some(256), Some(256)));
        assert!(!r.pixels_too_small(Some(640), Some(480)));
        assert!(!r.pixels_too_small(None, None));
    }
}

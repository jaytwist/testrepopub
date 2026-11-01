//! Disk identity. Files are stored as (volume, relative path) so that an external drive that comes
//! back as `F:` instead of `E:` is still recognized, and its index survives while it is unplugged.
//!
//! * Windows: identity is the volume serial number (stable across PCs and drive letters).
//! * Other OSes (development / tests): identity is the mount point path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct VolumeInfo {
    pub uid: String,
    pub label: String,
    pub root: PathBuf,
    pub removable: bool,
}

/// Path relative to the volume root, always with `/` separators.
pub fn rel_of(root: &Path, path: &Path) -> Result<String> {
    let rel = path
        .strip_prefix(root)
        .map_err(|_| anyhow!("{} is not under {}", path.display(), root.display()))?;
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    Ok(parts.join("/"))
}

pub fn join_rel(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in rel.split('/').filter(|s| !s.is_empty()) {
        p.push(part);
    }
    p
}

pub fn canonical(path: &Path) -> Result<PathBuf> {
    Ok(dunce::canonicalize(path)?)
}

/// Canonical form of a path that may not exist yet (canonicalize the deepest existing ancestor,
/// then re-append the rest). Used by previews, which must not create anything on disk.
pub fn canonical_lenient(path: &Path) -> Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match (existing.file_name().map(|s| s.to_os_string()), existing.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                existing = parent.to_path_buf();
            }
            _ => return Err(anyhow!("경로를 찾을 수 없습니다: {}", path.display())),
        }
    }
    let mut out = canonical(&existing)?;
    for part in tail.iter().rev() {
        out.push(part);
    }
    Ok(out)
}

/// Identify the volume a (possibly not yet existing) path will live on.
pub fn identify_lenient(path: &Path) -> Result<VolumeInfo> {
    let mut p = path.to_path_buf();
    while !p.exists() {
        if !p.pop() {
            return Err(anyhow!("경로를 찾을 수 없습니다: {}", path.display()));
        }
    }
    identify(&p)
}

// ---------------------------------------------------------------------------------------------
// Platform identification
// ---------------------------------------------------------------------------------------------

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, GetVolumePathNameW};

    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    pub fn identify(path: &Path) -> Result<VolumeInfo> {
        let path = canonical(path)?;
        let wpath = wide(path.as_os_str());
        let mut buf = [0u16; 512];
        unsafe { GetVolumePathNameW(PCWSTR(wpath.as_ptr()), &mut buf)? };
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let root_str = String::from_utf16_lossy(&buf[..len]);
        identify_root(&root_str)
    }

    fn identify_root(root_str: &str) -> Result<VolumeInfo> {
        let wroot: Vec<u16> = root_str.encode_utf16().chain(std::iter::once(0)).collect();
        let mut label = [0u16; 261];
        let mut serial: u32 = 0;
        let mut fsname = [0u16; 64];
        unsafe {
            GetVolumeInformationW(PCWSTR(wroot.as_ptr()), Some(&mut label), Some(&mut serial), None, None, Some(&mut fsname))?;
        }
        let label_len = label.iter().position(|&c| c == 0).unwrap_or(label.len());
        let label = String::from_utf16_lossy(&label[..label_len]);
        let dtype = unsafe { GetDriveTypeW(PCWSTR(wroot.as_ptr())) };
        let root = PathBuf::from(root_str);
        let display = if label.is_empty() { root_str.trim_end_matches('\\').to_string() } else { label };
        Ok(VolumeInfo {
            uid: format!("serial:{serial:08X}"),
            label: display,
            root,
            removable: dtype == DRIVE_REMOVABLE || dtype == DRIVE_CDROM,
        })
    }

    pub fn mounted() -> Vec<VolumeInfo> {
        let mask = unsafe { GetLogicalDrives() };
        let mut out = Vec::new();
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let root = format!("{}:\\", (b'A' + i as u8) as char);
            let wroot: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
            let dtype = unsafe { GetDriveTypeW(PCWSTR(wroot.as_ptr())) };
            // Mapped network drives (NAS) count too; CD/DVD and RAM disks are skipped.
            if dtype != DRIVE_FIXED && dtype != DRIVE_REMOVABLE && dtype != DRIVE_REMOTE {
                continue;
            }
            if let Ok(v) = identify_root(&root) {
                out.push(v);
            }
        }
        out
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// Walk up until the device id changes; that directory is the mount root.
    pub fn identify(path: &Path) -> Result<VolumeInfo> {
        let path = canonical(path)?;
        let dev = std::fs::metadata(&path)?.dev();
        let mut root = path.clone();
        while let Some(parent) = root.parent() {
            match std::fs::metadata(parent) {
                Ok(m) if m.dev() == dev => root = parent.to_path_buf(),
                _ => break,
            }
        }
        // Tests can force a "volume" boundary with a marker file, to simulate several drives.
        let mut probe = path.clone();
        loop {
            if probe.join(".mo-volume").is_file() {
                root = probe.clone();
                break;
            }
            if probe == root || !probe.pop() {
                break;
            }
        }
        let label = root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
        let s = root.to_string_lossy();
        let removable = s.starts_with("/media/") || s.starts_with("/run/media/") || s.starts_with("/mnt/");
        Ok(VolumeInfo { uid: format!("mount:{}", root.display()), label, root, removable })
    }

    pub fn mounted() -> Vec<VolumeInfo> {
        Vec::new()
    }
}

pub fn identify(path: &Path) -> Result<VolumeInfo> {
    platform::identify(path)
}

/// Currently mounted volumes, keyed by uid. On non-Windows this is empty and
/// `refresh` instead re-identifies each known mount path.
pub fn mounted() -> HashMap<String, VolumeInfo> {
    platform::mounted().into_iter().map(|v| (v.uid.clone(), v)).collect()
}

// ---------------------------------------------------------------------------------------------
// DB integration
// ---------------------------------------------------------------------------------------------

/// Insert or update a volume row; returns its id.
pub fn upsert(conn: &Connection, v: &VolumeInfo) -> Result<i64> {
    let now = crate::now_ts();
    let mount = v.root.to_string_lossy().into_owned();
    conn.execute(
        "INSERT INTO volumes(uid, label, mount, removable, online, last_seen) VALUES (?1, ?2, ?3, ?4, 1, ?5)
         ON CONFLICT(uid) DO UPDATE SET label = excluded.label, mount = excluded.mount,
             removable = excluded.removable, online = 1, last_seen = excluded.last_seen",
        params![v.uid, v.label, mount, v.removable as i64, now],
    )?;
    Ok(conn.query_row("SELECT id FROM volumes WHERE uid = ?", [&v.uid], |r| r.get(0))?)
}

/// Re-detect which known volumes are online and where they are mounted now.
pub fn refresh(conn: &Connection) -> Result<()> {
    let live = mounted();
    let mut st = conn.prepare("SELECT id, uid, mount FROM volumes")?;
    let known: Vec<(i64, String, String)> =
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    let now = crate::now_ts();
    for (id, uid, mount) in known {
        let found = if let Some(v) = live.get(&uid) {
            Some(v.clone())
        } else if live.is_empty() && Path::new(&mount).exists() {
            identify(Path::new(&mount)).ok().filter(|v| v.uid == uid)
        } else {
            None
        };
        match found {
            Some(v) => {
                conn.execute(
                    "UPDATE volumes SET online = 1, mount = ?, label = ?, last_seen = ? WHERE id = ?",
                    params![v.root.to_string_lossy(), v.label, now, id],
                )?;
            }
            None => {
                conn.execute("UPDATE volumes SET online = 0 WHERE id = ?", [id])?;
            }
        }
    }
    Ok(())
}

/// Register a folder as a scan source; returns the source id.
pub fn add_source(conn: &Connection, folder: &Path) -> Result<i64> {
    let folder = canonical(folder)?;
    if !folder.is_dir() {
        return Err(anyhow!("{} is not a folder", folder.display()));
    }
    let v = identify(&folder)?;
    let vid = upsert(conn, &v)?;
    let rel = rel_of(&v.root, &folder)?;
    // A source nested inside an existing source is redundant; a parent replaces its children.
    let existing: Vec<(i64, String)> = {
        let mut st = conn.prepare("SELECT id, rel_root FROM sources WHERE volume_id = ?")?;
        let rows = st.query_map([vid], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    for (id, r) in &existing {
        if is_within(&rel, r) {
            return Ok(*id);
        }
    }
    for (id, r) in &existing {
        if is_within(r, &rel) {
            conn.execute("DELETE FROM sources WHERE id = ?", [id])?;
        }
    }
    conn.execute("INSERT OR IGNORE INTO sources(volume_id, rel_root) VALUES (?, ?)", params![vid, rel])?;
    Ok(conn.query_row("SELECT id FROM sources WHERE volume_id = ? AND rel_root = ?", params![vid, rel], |r| r.get(0))?)
}

/// Stop tracking a source folder: drop its index rows (files on disk are untouched).
pub fn remove_source(conn: &Connection, source_id: i64) -> Result<()> {
    let (vid, root): (i64, String) =
        conn.query_row("SELECT volume_id, rel_root FROM sources WHERE id = ?", [source_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    conn.execute(
        "DELETE FROM files WHERE volume_id = ?1 AND quarantined = 0
           AND (?2 = '' OR substr(rel_path, 1, length(?2) + 1) = ?2 || '/')",
        params![vid, root],
    )?;
    conn.execute("DELETE FROM sources WHERE id = ?", [source_id])?;
    Ok(())
}

/// `child` is equal to or nested under `parent` (both volume-relative).
pub fn is_within(child: &str, parent: &str) -> bool {
    parent.is_empty() || child == parent || child.starts_with(&format!("{parent}/"))
}

pub fn volume_mount(conn: &Connection, volume_id: i64) -> Result<Option<(PathBuf, bool)>> {
    Ok(conn
        .query_row("SELECT mount, online FROM volumes WHERE id = ?", [volume_id], |r| {
            Ok((PathBuf::from(r.get::<_, String>(0)?), r.get::<_, i64>(1)? != 0))
        })
        .optional()?)
}

/// Where quarantined (soft-deleted) files go on a given volume. Same volume => instant rename,
/// and it never needs admin rights: on the system drive we use the user's profile folder.
pub fn quarantine_root(volume_root: &Path) -> PathBuf {
    if let Some(home) = home_dir() {
        if let (Ok(h), Ok(r)) = (identify(&home), identify(volume_root)) {
            if h.uid == r.uid {
                return home.join(".media-organizer").join("quarantine");
            }
        }
    }
    volume_root.join(QUARANTINE_DIR)
}

pub const QUARANTINE_DIR: &str = ".media-organizer-quarantine";

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_roundtrip() {
        let root = Path::new("/mnt/disk");
        let p = Path::new("/mnt/disk/Photos/2019/a.jpg");
        let rel = rel_of(root, p).unwrap();
        assert_eq!(rel, "Photos/2019/a.jpg");
        assert_eq!(join_rel(root, &rel), p);
    }

    #[test]
    fn within() {
        assert!(is_within("a/b", "a"));
        assert!(is_within("a", "a"));
        assert!(!is_within("ab", "a"));
        assert!(is_within("x", ""));
    }
}

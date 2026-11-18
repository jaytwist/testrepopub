//! Every file operation the app performs is journaled so it can be undone.
//!
//! * `quarantine`: soft delete. The file is renamed into a quarantine folder on the *same* volume
//!   (instant, no copy, works on FAT/exFAT USB drives that have no Recycle Bin). Purging the
//!   quarantine is the only permanent delete in the app.
//! * `move`: same volume = rename; cross volume = copy -> BLAKE3 verify -> delete source.
//! * `copy`: copy -> verify; the copy gets its own index row.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::{db, volumes, Db, Progress, ProgressFn, StopFlag};

pub fn new_batch(conn: &Connection, kind: &str, description: &str) -> Result<String> {
    let id = format!("{}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"), &uuid::Uuid::new_v4().simple().to_string()[..6]);
    conn.execute(
        "INSERT INTO batches(id, kind, description, created_at, state) VALUES (?, ?, ?, ?, 'running')",
        params![id, kind, description, crate::now_ts()],
    )?;
    Ok(id)
}

pub fn finish_batch(conn: &Connection, batch_id: &str) -> Result<()> {
    conn.execute("DELETE FROM journal WHERE batch_id = ? AND undone = 2", [batch_id])?;
    conn.execute(
        "UPDATE batches SET state = 'done', op_count = (SELECT COUNT(*) FROM journal WHERE batch_id = ?1) WHERE id = ?1",
        [batch_id],
    )?;
    Ok(())
}

/// A move the organizer is about to make (see `plan_moves`).
pub struct PlannedMove<'a> {
    pub file_id: i64,
    pub src: &'a str,
    pub dst: &'a str,
    pub dst_volume: i64,
    pub dst_rel: String,
}

/// Write down the moves of a batch before any of them is made. A move is a rename on disk and
/// then an index update saved with a few hundred others: if the app dies in between, the file is
/// at its new place and nothing says where it came from. With the move written down first,
/// `recover_interrupted` sees which of them happened and completes the record — the batch can
/// still be undone. (Rows with `undone = 2`: planned, not made yet.)
pub fn plan_moves(conn: &Connection, batch: &str, moves: &[PlannedMove]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut st = tx.prepare(
            "INSERT INTO journal(batch_id, op, file_id, src_abs, dst_abs, src_volume, src_rel, dst_volume, dst_rel, ts, undone)
             SELECT ?1, 'move', id, ?3, ?4, volume_id, rel_path, ?5, ?6, ?7, 2 FROM files WHERE id = ?2",
        )?;
        let now = crate::now_ts();
        for m in moves {
            st.execute(params![batch, m.file_id, m.src, m.dst, m.dst_volume, m.dst_rel, now])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Work left `running` by a crash, a closed window or power loss (call at start, when nothing
/// can be running). Returns the descriptions of the interrupted entries, which are closed and
/// marked 「중단됨」 so they can be undone. What was done on disk but not saved is put right first:
/// * a planned move that happened (the file is at its new place, not at the old one) is
///   recorded, so undoing the entry brings it back;
/// * a file found in an interrupted entry's quarantine folder with no record of it goes back to
///   where it was — never deleted, never left where nothing knows about it.
pub fn recover_interrupted(conn: &Connection) -> Result<Vec<String>> {
    volumes::refresh(conn)?;
    let adopted = adopt_planned_moves(conn)?;
    let restored = restore_unrecorded(conn)?;
    if adopted + restored > 0 {
        log::warn!("recovered after an interruption: {adopted} moves recorded, {restored} files put back from the quarantine");
    }
    conn.execute("DELETE FROM batches WHERE state = 'running' AND NOT EXISTS (SELECT 1 FROM journal j WHERE j.batch_id = batches.id)", [])?;
    let names: Vec<String> = {
        let mut st = conn.prepare("SELECT description FROM batches WHERE state = 'running' ORDER BY created_at")?;
        let rows = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    conn.execute(
        "UPDATE batches SET state = 'done', description = description || ' (중단됨)',
                op_count = (SELECT COUNT(*) FROM journal j WHERE j.batch_id = batches.id AND j.undone != 2)
         WHERE state = 'running'",
        [],
    )?;
    Ok(names)
}

/// Planned moves (`plan_moves`) left behind: those that were made are recorded, the others
/// dropped. One whose drive is not connected waits for a later start.
fn adopt_planned_moves(conn: &Connection) -> Result<u64> {
    type Row = (i64, i64, String, String, i64, String, i64, String);
    let rows: Vec<Row> = {
        let mut st = conn.prepare("SELECT id, file_id, src_abs, dst_abs, src_volume, src_rel, dst_volume, dst_rel FROM journal WHERE undone = 2")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    let mut adopted = 0;
    for (jid, file_id, src_abs, dst_abs, src_vol, src_rel, dst_vol, dst_rel) in rows {
        let online = |v: i64| matches!(volumes::volume_mount(conn, v), Ok(Some((m, true))) if m.is_dir());
        if !online(src_vol) || !online(dst_vol) {
            continue;
        }
        let (src, dst) = (at_now(conn, &src_abs, src_vol, &src_rel), at_now(conn, &dst_abs, dst_vol, &dst_rel));
        // Still where the index says (the move was not made, or was saved under another name).
        let row: Option<(i64, String, i64)> =
            conn.query_row("SELECT volume_id, rel_path, size FROM files WHERE id = ?", [file_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
        let made = match row {
            Some((v, rel, size)) if v == src_vol && rel == src_rel => {
                let taken: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM files WHERE volume_id = ? AND rel_path = ? AND id != ?)", params![dst_vol, dst_rel, file_id], |r| r.get(0))?;
                !taken && !src.exists() && fs::metadata(&dst).map(|m| m.is_file() && m.len() as i64 == size).unwrap_or(false)
            }
            _ => false,
        };
        if made {
            let name = dst.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            conn.execute("UPDATE files SET volume_id = ?, rel_path = ?, name = ? WHERE id = ?", params![dst_vol, dst_rel, name, file_id])?;
            conn.execute("UPDATE journal SET undone = 0 WHERE id = ?", [jid])?;
            adopted += 1;
        } else {
            conn.execute("DELETE FROM journal WHERE id = ?", [jid])?;
        }
    }
    Ok(adopted)
}

/// Files sitting in the quarantine folder of an interrupted batch that the journal does not
/// know: moved there, and the app died before saving it. Each goes back to its place.
fn restore_unrecorded(conn: &Connection) -> Result<u64> {
    let batches: Vec<String> = {
        let mut st = conn.prepare("SELECT id FROM batches WHERE state = 'running'")?;
        let rows = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    if batches.is_empty() {
        return Ok(0);
    }
    let vols: Vec<(i64, String)> = {
        let mut st = conn.prepare("SELECT id, mount FROM volumes WHERE online = 1")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    let mut restored = 0;
    for batch in &batches {
        for (vol, mount) in &vols {
            let mount = Path::new(mount);
            let held = volumes::quarantine_root(mount).join(batch).join(format!("v{vol}"));
            if !held.is_dir() {
                continue;
            }
            let mut stack = vec![held.clone()];
            while let Some(dir) = stack.pop() {
                for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let p = e.path();
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        stack.push(p);
                        continue;
                    }
                    let Ok(rel) = volumes::rel_of(&held, &p) else { continue };
                    let known: bool =
                        conn.query_row("SELECT EXISTS(SELECT 1 FROM journal WHERE batch_id = ? AND src_volume = ? AND src_rel = ? AND op IN ('quarantine', 'purged'))", params![batch, vol, rel], |r| r.get(0))?;
                    if known {
                        continue;
                    }
                    let back = volumes::join_rel(mount, &rel);
                    if let Some(d) = back.parent() {
                        let _ = fs::create_dir_all(d);
                    }
                    match rename_noclobber(&p, &back) {
                        Ok(()) => {
                            conn.execute("UPDATE files SET quarantined = 0, missing = 0 WHERE volume_id = ? AND rel_path = ?", params![vol, rel])?;
                            restored += 1;
                        }
                        Err(e) => log::warn!("could not put back {}: {e}", p.display()),
                    }
                }
            }
            // Folders left empty by this go; what the journal knows stays.
            let mut dirs = vec![held.clone()];
            let mut i = 0;
            while i < dirs.len() {
                for e in fs::read_dir(&dirs[i]).into_iter().flatten().flatten() {
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        dirs.push(e.path());
                    }
                }
                i += 1;
            }
            for d in dirs.iter().rev() {
                let _ = fs::remove_dir(d);
            }
            let _ = fs::remove_dir(held.parent().unwrap_or(&held));
        }
    }
    Ok(restored)
}

/// Where a file recorded as (volume, relative path) is right now. The journal also keeps the
/// full path it had then; a drive that came back under another letter is found by its volume.
fn at_now(conn: &Connection, stored: &str, vol: i64, rel: &str) -> PathBuf {
    let stored = PathBuf::from(stored);
    if vol == 0 || rel.is_empty() {
        return stored;
    }
    match volumes::volume_mount(conn, vol) {
        Ok(Some((mount, true))) if !stored.starts_with(&mount) => volumes::join_rel(&mount, rel),
        _ => stored,
    }
}

/// A quarantined file of a batch: where it is, or why it cannot be reached.
enum Held {
    At(PathBuf),
    /// Its drive is not connected: it is still there, to be dealt with later.
    Offline,
    /// The drive is here and the file is not in the quarantine (removed by hand).
    Gone,
}

fn held(conn: &Connection, batch: &str, stored: &str, vol: i64, rel: &str) -> Held {
    let stored = PathBuf::from(stored);
    if stored.exists() {
        return Held::At(stored);
    }
    match volumes::volume_mount(conn, vol) {
        Ok(Some((mount, true))) if mount.is_dir() => {
            let p = volumes::join_rel(&volumes::quarantine_root(&mount).join(batch), &format!("v{vol}/{rel}"));
            if p.exists() {
                Held::At(p)
            } else {
                Held::Gone
            }
        }
        _ => Held::Offline,
    }
}

// ---------------------------------------------------------------------------------------------
// Low-level file helpers
// ---------------------------------------------------------------------------------------------

pub fn hash_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().to_hex().to_string())
}

/// Hash of the first 64 KiB plus the size: cheap pre-filter before full hashing.
pub fn prehash_file(path: &Path, size: u64) -> Result<String> {
    let mut f = fs::File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut filled = 0;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    let mut h = blake3::Hasher::new();
    h.update(&size.to_le_bytes());
    h.update(&buf[..filled]);
    Ok(h.finalize().to_hex()[..32].to_string())
}

fn same_volume(a: &Path, b: &Path) -> bool {
    match (volumes::identify(a), volumes::identify(b)) {
        (Ok(x), Ok(y)) => x.uid == y.uid,
        _ => false,
    }
}

fn existing_ancestor(p: &Path) -> PathBuf {
    let mut cur = p.to_path_buf();
    while !cur.exists() {
        if !cur.pop() {
            break;
        }
    }
    cur
}

/// Rename `src` to `dst`, never replacing a file that is already there. (`std::fs::rename`
/// replaces it, on Windows too: "is it free?" followed by a rename leaves a moment in which
/// another program's file would be overwritten.)
pub fn rename_noclobber(src: &Path, dst: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVE_FILE_FLAGS};
        let wide = |p: &Path| -> Vec<u16> { verbatim(p).as_os_str().encode_wide().chain(std::iter::once(0)).collect() };
        let (s, d) = (wide(src), wide(dst));
        // No flags: fails if `dst` exists, and across drives (the caller copies then).
        // The system's own error code, so that "already exists" reads as that (the `windows`
        // crate's error carries an HRESULT, which `io::Error` does not understand).
        match unsafe { MoveFileExW(PCWSTR(s.as_ptr()), PCWSTR(d.as_ptr()), MOVE_FILE_FLAGS(0)) } {
            Ok(()) => Ok(()),
            Err(e) => {
                let hr = e.code().0 as u32;
                Err(if hr >> 16 == 0x8007 { std::io::Error::from_raw_os_error((hr & 0xFFFF) as i32) } else { std::io::Error::other(e) })
            }
        }
    }
    #[cfg(not(windows))]
    {
        if dst.symlink_metadata().is_ok() {
            return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, format!("{} exists", dst.display())));
        }
        fs::rename(src, dst)
    }
}

/// The `\\?\` form of an absolute path, which Windows takes at any length.
#[cfg(windows)]
fn verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy().replace('/', "\\");
    if s.starts_with(r"\\?\") || !p.is_absolute() {
        return p.to_path_buf();
    }
    match s.strip_prefix(r"\\") {
        Some(rest) => PathBuf::from(format!(r"\\?\UNC\{rest}")),
        None => PathBuf::from(format!(r"\\?\{s}")),
    }
}

/// Delete a file, also one marked read-only (Windows refuses those until the mark is cleared).
pub fn remove_file_force(p: &Path) -> std::io::Result<()> {
    match fs::remove_file(p) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            clear_readonly(p);
            fs::remove_file(p)
        }
        r => r,
    }
}

fn clear_readonly(p: &Path) {
    if let Ok(md) = fs::metadata(p) {
        let mut perm = md.permissions();
        if perm.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            let _ = fs::set_permissions(p, perm);
        }
    }
}

/// Copy with verification. Writes to a temp name first so a crash never leaves a half file at
/// `dst`; the copy is written out to the disk before it counts (a copy still in memory when the
/// power goes, with its source already deleted, would be a lost photo), read back and compared.
pub fn copy_verified(src: &Path, dst: &Path) -> Result<String> {
    if dst.exists() {
        bail!("대상 파일이 이미 존재합니다: {}", dst.display());
    }
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p)?;
    }
    let tmp = dst.with_file_name(format!(
        ".{}.mo-partial",
        dst.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    // (A partial copy an earlier, interrupted run left under this name is replaced.)
    let _ = remove_file_force(&tmp);
    let made: Result<String> = (|| {
        fs::copy(src, &tmp).with_context(|| format!("copy {} -> {}", src.display(), tmp.display()))?;
        // A read-only original makes a read-only copy, which could be neither written out nor
        // moved or deleted later.
        clear_readonly(&tmp);
        fs::OpenOptions::new().write(true).open(&tmp).and_then(|f| f.sync_all()).with_context(|| format!("write {}", tmp.display()))?;
        let (hs, hd) = (hash_file(src)?, hash_file(&tmp)?);
        if hs != hd {
            bail!("복사 검증 실패 (hash mismatch): {}", src.display());
        }
        if let Ok(md) = fs::metadata(src) {
            if let Ok(m) = md.modified() {
                let _ = filetime::set_file_mtime(&tmp, filetime::FileTime::from_system_time(m));
            }
        }
        rename_noclobber(&tmp, dst).with_context(|| format!("대상 파일이 이미 존재합니다: {}", dst.display()))?;
        Ok(hs)
    })();
    if made.is_err() {
        let _ = remove_file_force(&tmp);
        return made;
    }
    // The new name is on disk too (where the system lets a folder be written out).
    #[cfg(unix)]
    if let Some(d) = dst.parent() {
        let _ = fs::File::open(d).and_then(|f| f.sync_all());
    }
    made
}

/// Move a file; same-volume moves are a rename, cross-volume moves are copy+verify+delete.
pub fn move_file(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        bail!("대상 파일이 이미 존재합니다: {}", dst.display());
    }
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p)?;
    }
    if same_volume(src, &existing_ancestor(dst)) {
        match rename_noclobber(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!("대상 파일이 이미 존재합니다: {}", dst.display()),
            Err(e) => log::warn!("rename failed ({e}); falling back to copy"),
        }
    }
    copy_verified(src, dst)?;
    if let Err(e) = remove_file_force(src) {
        // The original cannot be removed (in use, no permission): it stays, and the copy just
        // made goes again — one file, where the index says it is, instead of two with one of
        // them unknown to the app.
        let _ = remove_file_force(dst);
        return Err(anyhow::Error::from(e).context(format!("원본을 옮길 수 없어 그대로 두었습니다: {}", src.display())));
    }
    Ok(())
}

/// Remove now-empty directories from `start` upward, stopping at `stop_at` (exclusive).
pub fn prune_empty_dirs(start: &Path, stop_at: &Path) {
    let mut cur = start.to_path_buf();
    while cur.starts_with(stop_at) && cur != stop_at {
        if fs::remove_dir(&cur).is_err() {
            break;
        }
        if !cur.pop() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Journaled operations
// ---------------------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn record(
    conn: &Connection,
    batch: &str,
    op: &str,
    file_id: i64,
    src: &Path,
    dst: &Path,
    src_vol: i64,
    src_rel: &str,
    dst_vol: i64,
    dst_rel: &str,
    new_file_id: Option<i64>,
) -> Result<()> {
    if op == "move" {
        let planned = conn.execute(
            "UPDATE journal SET undone = 0, src_abs = ?3, dst_abs = ?4, dst_volume = ?5, dst_rel = ?6, ts = ?7
             WHERE batch_id = ?1 AND file_id = ?2 AND op = 'move' AND undone = 2",
            params![batch, file_id, src.to_string_lossy(), dst.to_string_lossy(), dst_vol, dst_rel, crate::now_ts()],
        )?;
        if planned > 0 {
            return Ok(());
        }
    }
    conn.execute(
        "INSERT INTO journal(batch_id, op, file_id, src_abs, dst_abs, src_volume, src_rel, dst_volume, dst_rel, new_file_id, ts)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            batch, op, file_id, src.to_string_lossy(), dst.to_string_lossy(), src_vol, src_rel, dst_vol, dst_rel,
            new_file_id, crate::now_ts()
        ],
    )?;
    Ok(())
}

/// Move (or copy) a sidecar file (.xmp, .aae, …) with its photo `file_id`; undone together.
pub fn move_sidecar(conn: &Connection, batch: &str, file_id: i64, src: &Path, dst: &Path, copy: bool) -> Result<()> {
    if dst.exists() {
        bail!("이미 있음: {}", dst.display());
    }
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p)?;
    }
    if copy {
        fs::copy(src, dst)?;
    } else {
        move_file(src, dst)?;
    }
    record(conn, batch, if copy { "sidecar_copy" } else { "sidecar_move" }, file_id, src, dst, 0, "", 0, "", None)
}

/// Soft-delete one indexed file into its volume's quarantine folder.
pub fn quarantine(conn: &Connection, batch: &str, file: &db::FileRow) -> Result<()> {
    quarantine_rec(conn, batch, file, None)
}

/// Quarantine a copy whose identical twin `twin` stays. The twin is remembered so that emptying
/// the quarantine deletes the copy only after comparing it with the twin byte for byte.
pub fn quarantine_copy(conn: &Connection, batch: &str, file: &db::FileRow, twin: i64) -> Result<()> {
    quarantine_rec(conn, batch, file, Some(twin))
}

/// Quarantine a file whose place another file takes (an original replaced by a better copy).
/// Its index row lets go of the path — the new file needs it — and gets it back from the
/// journal if the quarantine is undone.
pub fn quarantine_parked(conn: &Connection, batch: &str, file: &db::FileRow) -> Result<()> {
    quarantine_rec(conn, batch, file, None)?;
    conn.execute("UPDATE files SET rel_path = ? WHERE id = ?", params![format!("\u{1}replaced/{batch}/{}", file.id), file.id])?;
    Ok(())
}

/// Put one quarantined file of a batch back where it was (the step that needed its place failed).
pub fn restore_quarantined(conn: &Connection, batch: &str, file_id: i64) -> Result<()> {
    let (jid, src, dst, vol, rel): (i64, String, String, i64, String) = conn.query_row(
        "SELECT id, src_abs, dst_abs, src_volume, src_rel FROM journal WHERE batch_id = ? AND file_id = ? AND op = 'quarantine' AND undone = 0",
        params![batch, file_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    unquarantine(conn, batch, file_id, Path::new(&src), Path::new(&dst), vol, &rel)?;
    conn.execute("DELETE FROM journal WHERE id = ?", [jid])?;
    Ok(())
}

/// Move a quarantined file back to `src` and give its row that place again. Refuses when
/// something else is there now: the file that took its place stays.
fn unquarantine(conn: &Connection, batch_id: &str, file_id: i64, src: &Path, dst: &Path, vol: i64, rel: &str) -> Result<()> {
    let taken: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM files WHERE volume_id = ? AND rel_path = ? AND id != ?)", params![vol, rel, file_id], |r| r.get(0))?;
    if src.exists() || taken {
        bail!("원래 위치에 같은 이름의 파일이 있습니다: {}", src.display());
    }
    if let Some(p) = src.parent() {
        fs::create_dir_all(p)?;
    }
    rename_noclobber(dst, src)?;
    let name = src.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    conn.execute("UPDATE files SET quarantined = 0, missing = 0, volume_id = ?, rel_path = ?, name = ? WHERE id = ?", params![vol, rel, name, file_id])?;
    if let Some(qroot) = dst.ancestors().find(|a| a.file_name().map(|n| n == batch_id).unwrap_or(false)) {
        prune_empty_dirs(dst.parent().unwrap_or(qroot), qroot.parent().unwrap_or(qroot));
    }
    Ok(())
}

fn quarantine_rec(conn: &Connection, batch: &str, file: &db::FileRow, twin: Option<i64>) -> Result<()> {
    if !file.online {
        bail!("드라이브가 연결되어 있지 않습니다: {}", file.name);
    }
    let src = file.abs_path();
    if !src.exists() {
        bail!("파일이 없습니다: {}", src.display());
    }
    let qroot = volumes::quarantine_root(Path::new(&file.mount));
    let dst = volumes::join_rel(&qroot.join(batch), &format!("v{}/{}", file.volume_id, file.rel_path));
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p)?;
    }
    rename_noclobber(&src, &dst).with_context(|| format!("quarantine {}", src.display()))?;
    conn.execute("UPDATE files SET quarantined = 1 WHERE id = ?", [file.id])?;
    record(conn, batch, "quarantine", file.id, &src, &dst, file.volume_id, &file.rel_path, file.volume_id, &file.rel_path, twin)?;
    Ok(())
}

/// How much of one file is read at a stretch before turning to the other. Two files on one
/// hard disk are far apart: reading them a megabyte at a time in turns keeps the head
/// travelling; longer stretches let it read. (Not longer than this: measured in memory, the
/// comparison itself slows down as the stretch outgrows the processor's cache — 16 MiB still
/// compares well over a gigabyte a second, and leaves a hard disk one move per 130 ms of reading.)
const RUN: usize = 16 << 20;
/// "Quick check": files up to this size are still compared whole; of larger ones the first and
/// the last `BLOCK` and one `BLOCK` in every `STRIPE` are compared.
const QUICK_WHOLE: u64 = 4 << 20;
const BLOCK: u64 = 1 << 20;
const STRIPE: u64 = 32 << 20;

/// Compares files byte for byte, reusing its two buffers from file to file.
pub struct Comparer {
    a: Vec<u8>,
    b: Vec<u8>,
    run: usize,
}

impl Default for Comparer {
    fn default() -> Self {
        Self::new()
    }
}

impl Comparer {
    pub fn new() -> Self {
        Self::with_run(RUN)
    }

    /// With another stretch length (tests use a small one to cross its boundaries).
    pub fn with_run(run: usize) -> Self {
        Self { a: Vec::new(), b: Vec::new(), run: run.max(1) }
    }

    fn buffers(&mut self, n: usize) {
        if self.a.len() < n {
            self.a.resize(n, 0);
            self.b.resize(n, 0);
        }
    }

    /// Whether the two files hold the same bytes, all of them.
    pub fn same(&mut self, a: &Path, b: &Path) -> Result<bool> {
        self.same_with(a, b, false, None)
    }

    /// `same`, and more: with `parallel` both files are read at the same time (they are on
    /// different drives — on one hard disk that would only make the head travel); `hash_b`
    /// takes in all of `b` on the way, so its full hash is known when the answer is "the same".
    pub fn same_with(&mut self, a: &Path, b: &Path, parallel: bool, mut hash_b: Option<&mut blake3::Hasher>) -> Result<bool> {
        let (la, lb) = (fs::metadata(a)?.len(), fs::metadata(b)?.len());
        if la != lb {
            return Ok(false);
        }
        self.buffers(la.min(self.run as u64) as usize);
        let (mut fa, mut fb) = (open_sequential(a)?, open_sequential(b)?);
        let mut left = la;
        while left > 0 {
            let want = left.min(self.run as u64) as usize;
            let (ba, bb) = (&mut self.a[..want], &mut self.b[..want]);
            let (x, y) = if parallel {
                std::thread::scope(|sc| {
                    let other = sc.spawn(|| read_full(&mut fb, bb));
                    let x = read_full(&mut fa, ba);
                    (x, other.join().unwrap_or_else(|_| Err(anyhow::anyhow!("read failed"))))
                })
            } else {
                (read_full(&mut fa, ba), read_full(&mut fb, bb))
            };
            let (x, y) = (x?, y?);
            if x != want || y != want || self.a[..want] != self.b[..want] {
                return Ok(false);
            }
            if let Some(h) = hash_b.as_deref_mut() {
                h.update(&self.b[..want]);
            }
            left -= want as u64;
        }
        Ok(true)
    }

    /// The quick check: whether the two files have the same size and the same bytes in the
    /// parts `sample_blocks` names. A file that differs only between those parts passes — that
    /// is the price of not reading it all.
    pub fn same_sampled(&mut self, a: &Path, b: &Path) -> Result<bool> {
        use std::io::{Seek, SeekFrom};
        let (la, lb) = (fs::metadata(a)?.len(), fs::metadata(b)?.len());
        if la != lb {
            return Ok(false);
        }
        if la <= QUICK_WHOLE {
            return self.same(a, b);
        }
        self.buffers(BLOCK as usize);
        let (mut fa, mut fb) = (fs::File::open(a)?, fs::File::open(b)?);
        for (at, len) in sample_blocks(la) {
            fa.seek(SeekFrom::Start(at))?;
            fb.seek(SeekFrom::Start(at))?;
            let x = read_full(&mut fa, &mut self.a[..len])?;
            let y = read_full(&mut fb, &mut self.b[..len])?;
            if x != len || y != len || self.a[..len] != self.b[..len] {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// The parts of a file of `len` bytes the quick check reads: (offset, length), in order. The
/// places inside the stripes follow from the length alone — the same for a file and its copy.
pub fn sample_blocks(len: u64) -> Vec<(u64, usize)> {
    if len <= QUICK_WHOLE {
        return vec![(0, len as usize)];
    }
    let mut at: Vec<u64> = vec![0, len - BLOCK];
    let mut seed = len ^ 0x9E37_79B9_7F4A_7C15;
    let mut stripe = 0;
    while stripe * STRIPE < len {
        // splitmix64: a different, fixed place in each stripe
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let start = stripe * STRIPE;
        let room = (len - start).min(STRIPE).saturating_sub(BLOCK);
        at.push((start + if room == 0 { 0 } else { z % room }).min(len - BLOCK));
        stripe += 1;
    }
    at.sort_unstable();
    at.dedup();
    at.into_iter().map(|o| (o, BLOCK as usize)).collect()
}

/// Open for reading from start to end (Windows then reads ahead in larger steps).
fn open_sequential(p: &Path) -> std::io::Result<fs::File> {
    let mut o = fs::OpenOptions::new();
    o.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        o.custom_flags(0x0800_0000); // FILE_FLAG_SEQUENTIAL_SCAN
    }
    o.open(p)
}

/// Whether two files hold the same bytes (all of them; see `Comparer`).
pub fn same_content(a: &Path, b: &Path) -> Result<bool> {
    Comparer::new().same(a, b)
}

fn read_full(f: &mut fs::File, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Move an indexed file to `dst` (absolute) and update its index row.
pub fn move_indexed(conn: &Connection, batch: &str, file: &db::FileRow, dst: &Path, dst_vol: i64, dst_rel: &str) -> Result<()> {
    let src = file.abs_path();
    move_file(&src, dst)?;
    conn.execute(
        "UPDATE files SET volume_id = ?, rel_path = ?, name = ? WHERE id = ?",
        params![dst_vol, dst_rel, dst.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(), file.id],
    )?;
    record(conn, batch, "move", file.id, &src, dst, file.volume_id, &file.rel_path, dst_vol, dst_rel, None)?;
    Ok(())
}

/// Copy an indexed file to `dst`, creating a new index row for the copy.
pub fn copy_indexed(conn: &Connection, batch: &str, file: &db::FileRow, dst: &Path, dst_vol: i64, dst_rel: &str) -> Result<i64> {
    let src = file.abs_path();
    let hash = copy_verified(&src, dst)?;
    let name = dst.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    conn.execute(
        "INSERT INTO files(volume_id, rel_path, name, ext, kind, size, mtime, taken_at, date_source, width, height,
                           duration_ms, camera, lat, lon, orientation, hash, prehash, missing, quarantined, scanned_at)
         SELECT ?1, ?2, ?3, ext, kind, size, mtime, taken_at, date_source, width, height, duration_ms, camera, lat, lon,
                orientation, ?4, prehash, 0, 0, scanned_at FROM files WHERE id = ?5
         ON CONFLICT(volume_id, rel_path) DO UPDATE SET missing = 0, quarantined = 0, hash = excluded.hash",
        params![dst_vol, dst_rel, name, hash, file.id],
    )?;
    let new_id: i64 = conn.query_row("SELECT id FROM files WHERE volume_id = ? AND rel_path = ?", params![dst_vol, dst_rel], |r| r.get(0))?;
    // The copy is byte-identical (verified above): it inherits analysis, faces, place, labels and
    // thumbnails instead of being read and analysed again.
    conn.execute(
        "INSERT OR IGNORE INTO labels(file_id, label, source) SELECT ?, label, source FROM labels WHERE file_id = ?",
        params![new_id, file.id],
    )?;
    crate::identity::clone_derived(conn, file.id, new_id)?;
    conn.execute("UPDATE files SET hash = ? WHERE id = ?", params![hash, file.id])?;
    record(conn, batch, "copy", file.id, &src, dst, file.volume_id, &file.rel_path, dst_vol, dst_rel, Some(new_id))?;
    Ok(new_id)
}

// ---------------------------------------------------------------------------------------------
// Batches: list / undo / purge
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct BatchRow {
    pub id: String,
    pub kind: String,
    pub description: String,
    pub created_at: i64,
    pub op_count: i64,
    pub state: String,
    pub bytes: i64,
    /// Files of this batch still waiting in the quarantine (not put back, not deleted).
    pub waiting: i64,
}

pub fn list_batches(conn: &Connection) -> Result<Vec<BatchRow>> {
    let mut st = conn.prepare(
        "SELECT b.id, b.kind, b.description, b.created_at, b.op_count, b.state,
                COALESCE((SELECT SUM(f.size) FROM journal j JOIN files f ON f.id = COALESCE(j.new_file_id, j.file_id) WHERE j.batch_id = b.id AND j.undone = 0), 0),
                (SELECT COUNT(*) FROM journal j WHERE j.batch_id = b.id AND j.op = 'quarantine' AND j.undone = 0)
         FROM batches b ORDER BY b.created_at DESC, b.id DESC",
    )?;
    let rows = st
        .query_map([], |r| {
            Ok(BatchRow {
                id: r.get(0)?,
                kind: r.get(1)?,
                description: r.get(2)?,
                created_at: r.get(3)?,
                op_count: r.get(4)?,
                state: r.get(5)?,
                bytes: r.get(6)?,
                waiting: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UndoReport {
    pub restored: u64,
    pub failed: u64,
    pub errors: Vec<String>,
    /// Stopped before the end: the rest is still as the batch left it.
    pub cancelled: bool,
    /// Bytes freed (emptying the quarantine).
    pub bytes: i64,
}

struct JournalEntry {
    id: i64,
    op: String,
    file_id: i64,
    src_abs: String,
    dst_abs: String,
    src_volume: i64,
    src_rel: String,
    dst_volume: i64,
    dst_rel: String,
    new_file_id: Option<i64>,
}

/// Reverse a batch, newest operation first.
pub fn undo_batch(conn: &Connection, batch_id: &str) -> Result<UndoReport> {
    undo_batch_with(conn, batch_id, &StopFlag::new(), &|_| {})
}

/// `undo_batch` as a job: says how far it is and can be stopped — what was not reached stays
/// done, and undoing again goes on from there.
pub fn undo_batch_with(conn: &Connection, batch_id: &str, stop: &StopFlag, progress: ProgressFn) -> Result<UndoReport> {
    let state: Option<String> = conn.query_row("SELECT state FROM batches WHERE id = ?", [batch_id], |r| r.get(0)).optional()?;
    match state.as_deref() {
        None => bail!("작업을 찾을 수 없습니다"),
        Some("purged") => bail!("격리함을 비운 작업은 되돌릴 수 없습니다"),
        Some("undone") => bail!("이미 되돌린 작업입니다"),
        _ => {}
    }
    // Drives may have come back under another letter since.
    volumes::refresh(conn)?;
    let entries: Vec<JournalEntry> = {
        let mut st = conn.prepare(
            "SELECT id, op, file_id, src_abs, dst_abs, src_volume, src_rel, dst_volume, dst_rel, new_file_id FROM journal
             WHERE batch_id = ? AND undone = 0 ORDER BY id DESC",
        )?;
        let rows = st
            .query_map([batch_id], |r| {
                Ok(JournalEntry {
                    id: r.get(0)?,
                    op: r.get(1)?,
                    file_id: r.get(2)?,
                    src_abs: r.get(3)?,
                    dst_abs: r.get(4)?,
                    src_volume: r.get(5)?,
                    src_rel: r.get(6)?,
                    dst_volume: r.get(7)?,
                    dst_rel: r.get(8)?,
                    new_file_id: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    let mut rep = UndoReport::default();
    let total = entries.len() as u64;
    for (i, e) in entries.into_iter().enumerate() {
        if stop.is_stopped() {
            rep.cancelled = true;
            break;
        }
        if i % 20 == 0 {
            progress(Progress::new("undo", "undo", i as u64, total, e.src_abs.clone()));
        }
        let src = at_now(conn, &e.src_abs, e.src_volume, &e.src_rel);
        let dst = if e.op == "quarantine" { PathBuf::from(&e.dst_abs) } else { at_now(conn, &e.dst_abs, e.dst_volume, &e.dst_rel) };
        let res: Result<()> = (|| {
            match e.op.as_str() {
                "quarantine" => match held(conn, batch_id, &e.dst_abs, e.src_volume, &e.src_rel) {
                    Held::At(dst) => unquarantine(conn, batch_id, e.file_id, &src, &dst, e.src_volume, &e.src_rel)?,
                    Held::Offline => bail!("드라이브가 연결되어 있지 않습니다: {}", e.src_abs),
                    Held::Gone => bail!("격리함에 파일이 없습니다: {}", e.dst_abs),
                },
                "move" => {
                    // Already back (an undo that was interrupted after the move): only the
                    // record is missing.
                    if dst.exists() || !src.exists() {
                        move_file(&dst, &src)?;
                    }
                    let name = src.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    conn.execute(
                        "UPDATE files SET volume_id = ?, rel_path = ?, name = ?, missing = 0 WHERE id = ?",
                        params![e.src_volume, e.src_rel, name, e.file_id],
                    )?;
                    if let Some(parent) = dst.parent() {
                        // Clean up folders the organizer created, up to the volume root.
                        if let Ok(v) = volumes::identify(&existing_ancestor(parent)) {
                            prune_empty_dirs(parent, &v.root);
                        }
                    }
                }
                "copy" => {
                    // The copy was moved on since (organized again, say): it is no longer at
                    // `dst`. Removing only its index entry would leave a file the app does not
                    // know about: leave both and say so.
                    if let Some(nid) = e.new_file_id {
                        if let Some(now_at) = db::get_file(conn, nid)?.map(|f| f.abs_path()) {
                            if now_at != dst && !dst.exists() {
                                bail!("가져온 파일이 그 뒤에 다른 곳으로 옮겨져 그대로 두었습니다: {}", now_at.display());
                            }
                        }
                    }
                    if dst.exists() {
                        let hs = hash_file(&src).ok();
                        let hd = hash_file(&dst)?;
                        if hs.as_deref() != Some(hd.as_str()) {
                            bail!("복사본이 수정되어 삭제하지 않았습니다: {}", dst.display());
                        }
                        fs::remove_file(&dst)?;
                    }
                    if let Some(nid) = e.new_file_id {
                        conn.execute("DELETE FROM files WHERE id = ?", [nid])?;
                    }
                    if let Some(parent) = dst.parent() {
                        if let Ok(v) = volumes::identify(&existing_ancestor(parent)) {
                            prune_empty_dirs(parent, &v.root);
                        }
                    }
                }
                "sidecar_move" => {
                    if dst.exists() && !src.exists() {
                        move_file(&dst, &src)?;
                    }
                }
                "sidecar_copy" => {
                    if dst.exists() {
                        fs::remove_file(&dst)?;
                    }
                }
                other => return Err(anyhow!("unknown op {other}")),
            }
            Ok(())
        })();
        match res {
            Ok(()) => {
                conn.execute("UPDATE journal SET undone = 1 WHERE id = ?", [e.id])?;
                rep.restored += 1;
            }
            Err(err) => {
                rep.failed += 1;
                rep.errors.push(err.to_string());
            }
        }
    }
    let state = if rep.failed == 0 && !rep.cancelled { "undone" } else { "done" };
    conn.execute("UPDATE batches SET state = ? WHERE id = ?", params![state, batch_id])?;
    progress(Progress::new("undo", "done", total, total, ""));
    Ok(rep)
}

/// Permanently delete quarantined files of a batch. This is the only irreversible action.
pub fn purge_batch(conn: &Connection, batch_id: &str) -> Result<UndoReport> {
    purge_batch_progress(conn, batch_id, false, &crate::StopFlag::new(), &|_| {})
}

/// `purge_batch` as a job: reports how far it is and can be stopped. What it had not reached
/// stays in the quarantine; running it again goes on from there.
pub fn purge_batch_progress(conn: &Connection, batch_id: &str, quick: bool, stop: &crate::StopFlag, progress: crate::ProgressFn) -> Result<UndoReport> {
    Ok(purge_summary(purge_batch_opts(conn, batch_id, quick, stop, progress)?))
}

/// Empty the whole quarantine as one job: every finished batch that still has files waiting,
/// oldest first, with one progress over all of them.
pub fn purge_all_progress(conn: &Connection, quick: bool, stop: &crate::StopFlag, progress: crate::ProgressFn) -> Result<UndoReport> {
    Ok(purge_summary(purge_all(conn, quick, stop, progress)?))
}

/// What a purge did, in the words the history page shows.
fn purge_summary(r: PurgeReport) -> UndoReport {
    let mut errors = r.errors;
    if r.offline > 0 {
        errors.insert(0, format!("연결되어 있지 않은 드라이브의 {}개는 지우지 못했습니다. 드라이브를 연결한 뒤 다시 누르세요", r.offline));
    }
    if r.kept > 0 {
        errors.insert(0, format!("원본과 내용이 달라(또는 원본이 없어) {}개는 격리함에 남겨 두었습니다", r.kept));
    }
    if r.cancelled {
        errors.insert(0, "중간에 멈췄습니다. 다시 누르면 남은 것부터 이어서 지웁니다".into());
    }
    UndoReport { restored: r.deleted, failed: r.failed + r.kept, errors, cancelled: r.cancelled, bytes: r.bytes }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PurgeReport {
    pub deleted: u64,
    pub bytes: i64,
    /// Copies left in the quarantine: their twin is gone or not byte-identical.
    pub kept: u64,
    pub failed: u64,
    pub errors: Vec<String>,
    pub cancelled: bool,
    /// Compared by the quick check (parts of each file) instead of all of it.
    pub quick: bool,
    /// Of the kept: copies compared with their twin and found different (copy, twin).
    pub differing: Vec<(i64, i64)>,
    /// Of the failed: files on a drive that is not connected (they wait for it).
    pub offline: u64,
    /// Copies deleted without reading them again: their full hash and their twin's were known.
    pub known: u64,
    /// Batches gone through (`purge_all`).
    pub batches: u64,
}

/// Permanently delete a batch's quarantined files. A copy quarantined with its twin
/// (`quarantine_copy`) is deleted only if the twin is still there with exactly the same bytes.
/// Deleted entries leave the journal's undo list; the batch counts as emptied once nothing is left.
pub fn purge_batch_with(conn: &Connection, batch_id: &str, stop: &crate::StopFlag, progress: crate::ProgressFn) -> Result<PurgeReport> {
    purge_batch_opts(conn, batch_id, false, stop, progress)
}

/// How far a purge is, over all the batches it goes through.
struct Tally {
    done: u64,
    total: u64,
    bytes_done: u64,
    bytes_total: u64,
    started: std::time::Instant,
}

const WAITING: &str = "FROM journal j LEFT JOIN files f ON f.id = j.file_id WHERE j.op = 'quarantine' AND j.undone = 0";

/// `purge_batch_with`; with `quick` the copy and its twin are compared in parts only
/// (`Comparer::same_sampled`): much less to read, and a difference between the parts is not seen.
pub fn purge_batch_opts(conn: &Connection, batch_id: &str, quick: bool, stop: &crate::StopFlag, progress: crate::ProgressFn) -> Result<PurgeReport> {
    volumes::refresh(conn)?;
    let (total, bytes_total): (i64, i64) =
        conn.query_row(&format!("SELECT COUNT(*), COALESCE(SUM(COALESCE(f.size, 0)), 0) {WAITING} AND j.batch_id = ?"), [batch_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let mut tally = Tally { done: 0, total: total as u64, bytes_done: 0, bytes_total: bytes_total.max(0) as u64, started: std::time::Instant::now() };
    let rep = purge_inner(conn, batch_id, quick, stop, progress, &mut tally)?;
    if !rep.cancelled {
        progress(crate::Progress::new("purge", stage_of(quick), tally.total, tally.total, "").with_bytes(tally.bytes_total, tally.bytes_total, tally.started.elapsed()));
    }
    Ok(rep)
}

/// Every finished batch with files still in the quarantine, emptied in one go (oldest first).
/// Stopped, it leaves the rest where it is; run again, it goes on from there.
pub fn purge_all(conn: &Connection, quick: bool, stop: &crate::StopFlag, progress: crate::ProgressFn) -> Result<PurgeReport> {
    volumes::refresh(conn)?;
    let of_done = "AND j.batch_id IN (SELECT id FROM batches WHERE state = 'done')";
    let batches: Vec<String> = {
        let mut st = conn.prepare(
            "SELECT b.id FROM batches b WHERE b.state = 'done' AND EXISTS(SELECT 1 FROM journal j WHERE j.batch_id = b.id AND j.op = 'quarantine' AND j.undone = 0)
             ORDER BY b.created_at, b.id",
        )?;
        let rows = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    let (total, bytes_total): (i64, i64) = conn.query_row(&format!("SELECT COUNT(*), COALESCE(SUM(COALESCE(f.size, 0)), 0) {WAITING} {of_done}"), [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let mut tally = Tally { done: 0, total: total as u64, bytes_done: 0, bytes_total: bytes_total.max(0) as u64, started: std::time::Instant::now() };
    let mut all = PurgeReport { quick, ..Default::default() };
    for b in &batches {
        if stop.is_stopped() {
            all.cancelled = true;
            break;
        }
        let r = purge_inner(conn, b, quick, stop, progress, &mut tally)?;
        all.batches += 1;
        all.deleted += r.deleted;
        all.bytes += r.bytes;
        all.kept += r.kept;
        all.failed += r.failed;
        all.offline += r.offline;
        all.known += r.known;
        all.differing.extend(r.differing);
        let room = 200usize.saturating_sub(all.errors.len());
        all.errors.extend(r.errors.into_iter().take(room));
        if r.cancelled {
            all.cancelled = true;
            break;
        }
    }
    if !all.cancelled {
        progress(crate::Progress::new("purge", stage_of(quick), tally.total, tally.total, "").with_bytes(tally.bytes_total, tally.bytes_total, tally.started.elapsed()));
    }
    Ok(all)
}

fn stage_of(quick: bool) -> &'static str {
    if quick {
        "verify_quick"
    } else {
        "verify"
    }
}

/// Database writes of a long run, saved together: every `EVERY` writes or `SPAN` of time, and
/// before anything that takes long (so no other writer waits for a file being read). Saving
/// each file on its own made the disk wait for every single one.
struct Saved<'a> {
    conn: &'a Connection,
    /// Not inside a transaction of the caller's.
    own: bool,
    open: bool,
    n: u32,
    since: std::time::Instant,
}

impl<'a> Saved<'a> {
    const EVERY: u32 = 200;
    const SPAN: std::time::Duration = std::time::Duration::from_secs(2);

    fn new(conn: &'a Connection) -> Self {
        Self { conn, own: conn.is_autocommit(), open: false, n: 0, since: std::time::Instant::now() }
    }

    /// Before writing.
    fn write(&mut self) -> Result<()> {
        if self.own && !self.open {
            self.conn.execute_batch("BEGIN IMMEDIATE")?;
            (self.open, self.n, self.since) = (true, 0, std::time::Instant::now());
        }
        self.n += 1;
        Ok(())
    }

    /// Save what is due — or, with `now`, whatever there is.
    fn save(&mut self, now: bool) -> Result<()> {
        if self.open && (now || self.n >= Self::EVERY || self.since.elapsed() >= Self::SPAN) {
            self.conn.execute_batch("COMMIT")?;
            self.open = false;
        }
        Ok(())
    }
}

fn mtime_secs(md: &fs::Metadata) -> i64 {
    md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The file's full hash from the index, if the file on disk is still the one it was taken
/// from (same size, same time): then there is no need to read it again.
fn known_hash<'a>(row: &'a db::FileRow, at: &Path) -> Option<&'a str> {
    let md = fs::metadata(at).ok()?;
    (md.len() as i64 == row.size && mtime_secs(&md) == row.mtime).then_some(row.hash.as_deref()).flatten()
}

/// Whether the quarantined copy at `p` holds what its twin holds, reading as little as that
/// takes: nothing if both full hashes are known, one file if one is, both otherwise (at the
/// same time when they are on different drives). `Ok((same, twin's path, read nothing))`.
fn copy_matches(conn: &Connection, cmp: &mut Comparer, quick: bool, saved: &mut Saved, copy_id: i64, copy_vol: i64, p: &Path, twin: i64) -> Result<(bool, Option<PathBuf>, bool)> {
    let Some(t) = db::get_file(conn, twin)?.filter(|f| !f.quarantined && !f.missing && f.online) else { return Ok((false, None, false)) };
    let tp = t.abs_path();
    let c = db::get_file(conn, copy_id)?;
    let th = known_hash(&t, &tp);
    let ch = c.as_ref().and_then(|c| known_hash(c, p));
    let keep_hash = |saved: &mut Saved, h: &str| -> Result<()> {
        // For the next copy of the same original: it alone is read then.
        if fs::metadata(&tp).map(|m| m.len() as i64 == t.size && mtime_secs(&m) == t.mtime).unwrap_or(false) {
            saved.write()?;
            conn.execute("UPDATE files SET hash = ? WHERE id = ? AND hash IS NULL", params![h, twin])?;
        }
        Ok(())
    };
    let (same, unread) = match (ch, th) {
        (Some(a), Some(b)) => (a == b, true),
        _ if quick => (cmp.same_sampled(p, &tp).unwrap_or(false), false),
        (None, Some(b)) => (hash_file(p).map(|h| h == b).unwrap_or(false), false),
        (Some(a), None) => match hash_file(&tp) {
            Ok(h) if h == a => {
                keep_hash(saved, &h)?;
                (true, false)
            }
            _ => (false, false),
        },
        (None, None) => {
            let mut h = blake3::Hasher::new();
            let same = cmp.same_with(p, &tp, t.volume_id != copy_vol, Some(&mut h)).unwrap_or(false);
            if same {
                keep_hash(saved, h.finalize().to_hex().as_str())?;
            }
            (same, false)
        }
    };
    Ok((same, Some(tp), unread))
}

/// Files this large are not read while database writes are waiting to be saved.
const LONG_READ: i64 = 32 << 20;
/// Copies smaller than this are checked in groups: first the copies of a group are read, one
/// after the other, then their originals. Read in turns — copy, original, copy, original — a
/// hard disk moves its head between two far places for every single photo.
const GROUPED_BELOW: i64 = 16 << 20;
const GROUP_FILES: usize = 400;
const GROUP_BYTES: i64 = 256 << 20;

struct Waiting {
    jid: i64,
    file_id: i64,
    dst: String,
    twin: Option<i64>,
    size: i64,
    vol: i64,
    rel: String,
}

/// How a waiting copy is checked against its original.
enum Check {
    /// Nothing to compare with (no original was named), or nothing there to compare.
    None,
    /// On its own, by `copy_matches` (large files, the quick check).
    Alone(i64),
    /// By full hashes, read in a group. `orig` is the original's path if it can be read.
    Hashes { twin: i64, orig: Option<PathBuf>, copy: Option<String>, unread: bool },
}

fn purge_inner(conn: &Connection, batch_id: &str, quick: bool, stop: &crate::StopFlag, progress: crate::ProgressFn, tally: &mut Tally) -> Result<PurgeReport> {
    let entries: Vec<Waiting> = {
        let mut st = conn.prepare(
            "SELECT j.id, j.file_id, j.dst_abs, j.new_file_id, COALESCE(f.size, 0), j.src_volume, j.src_rel FROM journal j LEFT JOIN files f ON f.id = j.file_id
             WHERE j.batch_id = ? AND j.op = 'quarantine' AND j.undone = 0 ORDER BY j.id",
        )?;
        let rows = st
            .query_map([batch_id], |r| Ok(Waiting { jid: r.get(0)?, file_id: r.get(1)?, dst: r.get(2)?, twin: r.get(3)?, size: r.get(4)?, vol: r.get(5)?, rel: r.get(6)? }))?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    let mut rep = PurgeReport { quick, ..Default::default() };
    let mut run = PurgeRun { conn, batch_id, quick, stop, progress, cmp: Comparer::new(), saved: Saved::new(conn), emptied: None };
    let done = (|| -> Result<()> {
        let mut at = 0;
        while at < entries.len() && !rep.cancelled {
            // The next group: up to so many files and bytes.
            let (mut end, mut bytes) = (at, 0i64);
            while end < entries.len() && end - at < GROUP_FILES && bytes < GROUP_BYTES {
                bytes += if entries[end].size < GROUPED_BELOW { entries[end].size.max(0) } else { 0 };
                end += 1;
            }
            run.group(&entries[at..end], &mut rep, tally)?;
            at = end;
        }
        Ok(())
    })();
    // Whatever was deleted is recorded, also when the run ends in an error.
    run.saved.save(true)?;
    run.tidy();
    done?;
    let left: i64 = conn.query_row("SELECT COUNT(*) FROM journal WHERE batch_id = ? AND undone = 0", [batch_id], |r| r.get(0))?;
    if left == 0 {
        conn.execute("UPDATE batches SET state = 'purged' WHERE id = ?", [batch_id])?;
    }
    Ok(rep)
}

struct PurgeRun<'a> {
    conn: &'a Connection,
    batch_id: &'a str,
    quick: bool,
    stop: &'a crate::StopFlag,
    progress: crate::ProgressFn<'a>,
    cmp: Comparer,
    saved: Saved<'a>,
    /// The folder the last deleted file was in, and where tidying up stops: emptied folders are
    /// removed when the run moves on to another folder, not after every file.
    emptied: Option<(PathBuf, PathBuf)>,
}

impl PurgeRun<'_> {
    fn tidy(&mut self) {
        if let Some((dir, top)) = self.emptied.take() {
            prune_empty_dirs(&dir, &top);
        }
    }

    fn group(&mut self, items: &[Waiting], rep: &mut PurgeReport, tally: &mut Tally) -> Result<()> {
        let conn = self.conn;
        // Where each file is now (its drive may have another letter), and how it is checked.
        // A drive that is not connected is not "nothing left to delete": the file waits for it.
        let mut at: Vec<Held> = Vec::with_capacity(items.len());
        let mut checks: Vec<Check> = Vec::with_capacity(items.len());
        // Originals whose full hash has to be read: id → (path, row).
        let mut to_read: std::collections::HashMap<i64, (PathBuf, db::FileRow)> = Default::default();
        let mut orig_hash: std::collections::HashMap<i64, Option<String>> = Default::default();
        for it in items {
            let h = held(conn, self.batch_id, &it.dst, it.vol, &it.rel);
            let check = match (&h, it.twin) {
                (Held::At(_), Some(twin)) if self.quick || it.size >= GROUPED_BELOW => Check::Alone(twin),
                (Held::At(p), Some(twin)) => match db::get_file(conn, twin)?.filter(|f| !f.quarantined && !f.missing && f.online) {
                    None => Check::Hashes { twin, orig: None, copy: None, unread: false },
                    Some(t) => {
                        let tp = t.abs_path();
                        let copy = db::get_file(conn, it.file_id)?.as_ref().and_then(|c| known_hash(c, p)).map(str::to_string);
                        let known = known_hash(&t, &tp).map(str::to_string);
                        let unread = copy.is_some() && known.is_some();
                        match known {
                            Some(k) => {
                                orig_hash.insert(twin, Some(k));
                            }
                            None => {
                                to_read.entry(twin).or_insert((tp.clone(), t));
                            }
                        }
                        Check::Hashes { twin, orig: Some(tp), copy, unread }
                    }
                },
                _ => Check::None,
            };
            at.push(h);
            checks.push(check);
        }
        // The copies, one after the other …
        for (h, check) in at.iter().zip(checks.iter_mut()) {
            if let (Held::At(p), Check::Hashes { orig: Some(_), copy: copy @ None, .. }) = (h, check) {
                if self.stop.is_stopped() {
                    rep.cancelled = true;
                    return Ok(());
                }
                *copy = hash_file(p).ok();
            }
        }
        // … then their originals, in the order they lie in their folders. What is read is kept
        // for the next copy of the same original.
        let mut originals: Vec<(i64, (PathBuf, db::FileRow))> = to_read.into_iter().collect();
        originals.sort_by(|a, b| a.1 .0.cmp(&b.1 .0));
        for (twin, (tp, row)) in originals {
            if self.stop.is_stopped() {
                rep.cancelled = true;
                return Ok(());
            }
            let h = hash_file(&tp).ok();
            if let Some(h) = &h {
                if fs::metadata(&tp).map(|m| m.len() as i64 == row.size && mtime_secs(&m) == row.mtime).unwrap_or(false) {
                    self.saved.write()?;
                    conn.execute("UPDATE files SET hash = ? WHERE id = ? AND hash IS NULL", params![h, twin])?;
                }
            }
            orig_hash.insert(twin, h);
        }
        self.saved.save(false)?;
        // The answers, file by file.
        let stage = stage_of(self.quick);
        for ((it, h), check) in items.iter().zip(at).zip(checks) {
            if self.stop.is_stopped() {
                rep.cancelled = true;
                return Ok(());
            }
            (self.progress)(crate::Progress::new("purge", stage, tally.done, tally.total, it.dst.clone()).with_bytes(tally.bytes_done, tally.bytes_total, tally.started.elapsed()));
            tally.done += 1;
            tally.bytes_done += it.size.max(0) as u64;
            // Debug builds only: a slow disk on demand (`MO_SLOW_COMPARE_MS` per file), to see the
            // progress a real run of hours shows.
            #[cfg(debug_assertions)]
            if let Some(ms) = std::env::var("MO_SLOW_COMPARE_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
            let (p, there) = match h {
                Held::At(p) => (p, true),
                Held::Gone => (PathBuf::from(&it.dst), false),
                Held::Offline => {
                    rep.failed += 1;
                    rep.offline += 1;
                    continue;
                }
            };
            // (same, the original's path if it could be read, read nothing)
            let verdict: Option<(bool, Option<PathBuf>, i64, bool)> = match check {
                Check::None => None,
                Check::Alone(twin) => {
                    self.saved.save(it.size >= LONG_READ)?;
                    let (same, tp, unread) = copy_matches(conn, &mut self.cmp, self.quick, &mut self.saved, it.file_id, it.vol, &p, twin)?;
                    Some((same, tp, twin, unread))
                }
                Check::Hashes { twin, orig, copy, unread } => {
                    let same = match (&copy, orig_hash.get(&twin).and_then(|h| h.as_ref())) {
                        (Some(c), Some(o)) => orig.is_some() && c == o,
                        _ => false,
                    };
                    Some((same, orig, twin, unread))
                }
            };
            if let Some((same, twin_path, twin, unread)) = verdict {
                if !same {
                    if let Some(tp) = &twin_path {
                        rep.differing.push((it.file_id, twin));
                        // The two only looked the same (same size, same beginning): from now on
                        // their fingerprints tell them apart, so neither counts as a copy of the
                        // other again. (Both are read for it: nothing waits to be saved meanwhile.)
                        self.saved.save(true)?;
                        let _ = mark_different(conn, it.file_id, &p, twin, tp);
                    }
                    rep.kept += 1;
                    if rep.errors.len() < 200 {
                        rep.errors.push(format!("{}: 원본과 내용이 같은지 확인하지 못해 남겨 둠", it.dst));
                    }
                    continue;
                }
                rep.known += unread as u64;
            }
            let size = fs::metadata(&p).map(|m| m.len() as i64).unwrap_or(0);
            let r = if there { remove_file_force(&p).map_err(anyhow::Error::from) } else { Ok(()) };
            match r {
                Ok(()) => {
                    self.saved.write()?;
                    conn.execute("DELETE FROM files WHERE id = ? AND quarantined = 1", [it.file_id])?;
                    conn.execute("UPDATE journal SET op = 'purged', undone = 1 WHERE id = ?", [it.jid])?;
                    rep.deleted += 1;
                    rep.bytes += size;
                    if let Some(qroot) = p.ancestors().find(|a| a.file_name().map(|n| n == self.batch_id).unwrap_or(false)) {
                        let here = (p.parent().unwrap_or(qroot).to_path_buf(), qroot.parent().unwrap_or(qroot).to_path_buf());
                        if self.emptied.as_ref() != Some(&here) {
                            self.tidy();
                            self.emptied = Some(here);
                        }
                    }
                }
                Err(e) => {
                    rep.failed += 1;
                    if rep.errors.len() < 200 {
                        rep.errors.push(format!("{}: {e}", it.dst));
                    }
                }
            }
            self.saved.save(false)?;
        }
        Ok(())
    }
}

/// Two files that share a fingerprint (same size, same first 64 KiB) and were found to differ:
/// each gets its full hash, and a fingerprint extended by it.
fn mark_different(conn: &Connection, a: i64, pa: &Path, b: i64, pb: &Path) -> Result<()> {
    let (ha, hb) = (hash_file(pa)?, hash_file(pb)?);
    if ha == hb {
        return Ok(());
    }
    for (id, h) in [(a, &ha), (b, &hb)] {
        conn.execute(
            "UPDATE files SET hash = ?1, prehash = CASE WHEN prehash IS NULL THEN NULL
                 ELSE (CASE WHEN instr(prehash, '~') > 0 THEN substr(prehash, 1, instr(prehash, '~') - 1) ELSE prehash END) || '~' || substr(?1, 1, 16) END
             WHERE id = ?2",
            params![h, id],
        )?;
    }
    Ok(())
}

/// A file with the same fingerprint as `id` that stays when `doomed` are deleted (one on a
/// connected drive if there is one).
fn surviving_twin(conn: &Connection, id: i64, doomed: &std::collections::HashSet<i64>) -> Result<Option<i64>> {
    let mut st = conn.prepare_cached(
        "SELECT f.id FROM files f JOIN volumes v ON v.id = f.volume_id
         WHERE COALESCE(f.prehash, f.hash) = (SELECT COALESCE(prehash, hash) FROM files WHERE id = ?1)
           AND f.id != ?1 AND f.missing = 0 AND f.quarantined = 0
         ORDER BY v.online DESC, f.id LIMIT 200",
    )?;
    let ids = st.query_map([id], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids.into_iter().find(|t| !doomed.contains(t)))
}

// ---------------------------------------------------------------------------------------------
// Delete selected files (soft: quarantine, undoable)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize)]
pub struct DeleteReport {
    pub batch_id: String,
    pub deleted: u64,
    pub bytes: i64,
    /// On a drive that is not connected right now.
    pub skipped_offline: u64,
    /// Protected (see `protect`) and not forced: left in place.
    pub protected: u64,
    pub failed: u64,
    pub errors: Vec<String>,
    pub cancelled: bool,
}

/// Move the given files into their drives' quarantine folders as one undoable batch.
/// Protected files (originals, ♥, keep folders, the last copy of some content) are left in place
/// unless `force`.
/// `label` names the batch in the history (e.g. "연사 정리").
pub fn delete_files(db: &Db, ids: &[i64], force: bool, label: Option<&str>, stop: &StopFlag, progress: ProgressFn) -> Result<DeleteReport> {
    let conn = db.conn()?;
    volumes::refresh(&conn)?;
    let protected: std::collections::HashSet<i64> =
        if force { Default::default() } else { crate::protect::check(&conn, ids)?.into_iter().map(|b| b.id).collect() };
    let what = match label {
        Some(l) => format!("{l} {}개 정리", ids.len()),
        None => format!("선택한 {}개 삭제", ids.len()),
    };
    let batch = new_batch(&conn, "delete", &what)?;
    let mut rep = DeleteReport { batch_id: batch.clone(), ..Default::default() };
    let total = ids.len() as u64;
    // What goes, to tell which copies stay.
    let doomed: std::collections::HashSet<i64> = ids.iter().copied().filter(|id| !protected.contains(id)).collect();
    for (i, id) in ids.iter().enumerate() {
        if stop.is_stopped() {
            rep.cancelled = true;
            break;
        }
        if i % 20 == 0 {
            progress(Progress::new("delete", "quarantine", i as u64, total, ""));
        }
        if protected.contains(id) {
            rep.protected += 1;
            continue;
        }
        let Some(f) = db::get_file(&conn, *id)? else { continue };
        if f.quarantined || f.missing {
            continue;
        }
        if !f.online {
            rep.skipped_offline += 1;
            continue;
        }
        // "Another copy stays" is known from the fingerprint only (size and first 64 KiB): a
        // copy cut short or damaged further in has the same one. So the copy that stays is
        // remembered, and emptying the quarantine compares the two byte for byte first — a
        // file that turns out to be the only good one is kept.
        let res = match surviving_twin(&conn, f.id, &doomed)? {
            Some(t) => quarantine_copy(&conn, &batch, &f, t),
            None => quarantine(&conn, &batch, &f),
        };
        match res {
            Ok(()) => {
                rep.deleted += 1;
                rep.bytes += f.size;
            }
            Err(e) => {
                rep.failed += 1;
                if rep.errors.len() < 100 {
                    rep.errors.push(e.to_string());
                }
            }
        }
    }
    finish_batch(&conn, &batch)?;
    if rep.deleted == 0 {
        conn.execute("DELETE FROM batches WHERE id = ? AND op_count = 0", [&batch])?;
    }
    progress(Progress::new("delete", "done", total, total, ""));
    Ok(rep)
}

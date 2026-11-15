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
    conn.execute(
        "UPDATE batches SET state = 'done', op_count = (SELECT COUNT(*) FROM journal WHERE batch_id = ?1) WHERE id = ?1",
        [batch_id],
    )?;
    Ok(())
}

/// Work left `running` by a crash or power loss (call at start, when nothing can be running):
/// entries that did nothing are dropped, the others are closed and marked 「중단됨」 so they can
/// be undone. Returns their descriptions. File moves made after the last saved step are found
/// again by the next scan (by content).
pub fn recover_interrupted(conn: &Connection) -> Result<Vec<String>> {
    conn.execute("DELETE FROM batches WHERE state = 'running' AND NOT EXISTS (SELECT 1 FROM journal j WHERE j.batch_id = batches.id)", [])?;
    let names: Vec<String> = {
        let mut st = conn.prepare("SELECT description FROM batches WHERE state = 'running' ORDER BY created_at")?;
        let rows = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    conn.execute(
        "UPDATE batches SET state = 'done', description = description || ' (중단됨)',
                op_count = (SELECT COUNT(*) FROM journal j WHERE j.batch_id = batches.id)
         WHERE state = 'running'",
        [],
    )?;
    Ok(names)
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

/// Copy with verification. Writes to a temp name first so a crash never leaves a half file at `dst`.
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
    fs::copy(src, &tmp).with_context(|| format!("copy {} -> {}", src.display(), tmp.display()))?;
    let (hs, hd) = (hash_file(src)?, hash_file(&tmp)?);
    if hs != hd {
        let _ = fs::remove_file(&tmp);
        bail!("복사 검증 실패 (hash mismatch): {}", src.display());
    }
    if let Ok(md) = fs::metadata(src) {
        if let Ok(m) = md.modified() {
            let _ = filetime::set_file_mtime(&tmp, filetime::FileTime::from_system_time(m));
        }
    }
    fs::rename(&tmp, dst)?;
    Ok(hs)
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
        match fs::rename(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) => log::warn!("rename failed ({e}); falling back to copy"),
        }
    }
    copy_verified(src, dst)?;
    fs::remove_file(src).with_context(|| format!("remove {}", src.display()))?;
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
    fs::rename(&src, &dst).with_context(|| format!("quarantine {}", src.display()))?;
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
        let (la, lb) = (fs::metadata(a)?.len(), fs::metadata(b)?.len());
        if la != lb {
            return Ok(false);
        }
        self.buffers(la.min(self.run as u64) as usize);
        let (mut fa, mut fb) = (open_sequential(a)?, open_sequential(b)?);
        let mut left = la;
        while left > 0 {
            let want = left.min(self.run as u64) as usize;
            let x = read_full(&mut fa, &mut self.a[..want])?;
            let y = read_full(&mut fb, &mut self.b[..want])?;
            if x != want || y != want || self.a[..want] != self.b[..want] {
                return Ok(false);
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
}

struct JournalEntry {
    id: i64,
    op: String,
    file_id: i64,
    src_abs: String,
    dst_abs: String,
    src_volume: i64,
    src_rel: String,
    new_file_id: Option<i64>,
}

/// Reverse a batch, newest operation first.
pub fn undo_batch(conn: &Connection, batch_id: &str) -> Result<UndoReport> {
    let state: Option<String> = conn.query_row("SELECT state FROM batches WHERE id = ?", [batch_id], |r| r.get(0)).optional()?;
    match state.as_deref() {
        None => bail!("작업을 찾을 수 없습니다"),
        Some("purged") => bail!("격리함을 비운 작업은 되돌릴 수 없습니다"),
        Some("undone") => bail!("이미 되돌린 작업입니다"),
        _ => {}
    }
    let entries: Vec<JournalEntry> = {
        let mut st = conn.prepare(
            "SELECT id, op, file_id, src_abs, dst_abs, src_volume, src_rel, new_file_id FROM journal
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
                    new_file_id: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    let mut rep = UndoReport::default();
    for e in entries {
        let (src, dst) = (PathBuf::from(&e.src_abs), PathBuf::from(&e.dst_abs));
        let res: Result<()> = (|| {
            match e.op.as_str() {
                "quarantine" => {
                    if src.exists() {
                        bail!("원래 위치에 같은 이름의 파일이 있습니다: {}", src.display());
                    }
                    if let Some(p) = src.parent() {
                        fs::create_dir_all(p)?;
                    }
                    fs::rename(&dst, &src)?;
                    conn.execute("UPDATE files SET quarantined = 0, missing = 0 WHERE id = ?", [e.file_id])?;
                    if let Some(qroot) = dst.ancestors().find(|a| a.file_name().map(|n| n == batch_id).unwrap_or(false)) {
                        prune_empty_dirs(dst.parent().unwrap_or(qroot), qroot.parent().unwrap_or(qroot));
                    }
                }
                "move" => {
                    move_file(&dst, &src)?;
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
    let state = if rep.failed == 0 { "undone" } else { "done" };
    conn.execute("UPDATE batches SET state = ? WHERE id = ?", params![state, batch_id])?;
    Ok(rep)
}

/// Permanently delete quarantined files of a batch. This is the only irreversible action.
pub fn purge_batch(conn: &Connection, batch_id: &str) -> Result<UndoReport> {
    purge_batch_progress(conn, batch_id, &crate::StopFlag::new(), &|_| {})
}

/// `purge_batch` as a job: reports how far it is and can be stopped. What it had not reached
/// stays in the quarantine; running it again goes on from there.
pub fn purge_batch_progress(conn: &Connection, batch_id: &str, stop: &crate::StopFlag, progress: crate::ProgressFn) -> Result<UndoReport> {
    let r = purge_batch_with(conn, batch_id, stop, progress)?;
    let mut errors = r.errors;
    if r.kept > 0 {
        errors.insert(0, format!("원본과 내용이 달라(또는 원본이 없어) {}개는 격리함에 남겨 두었습니다", r.kept));
    }
    if r.cancelled {
        errors.insert(0, "중간에 멈췄습니다. 다시 누르면 남은 것부터 이어서 지웁니다".into());
    }
    Ok(UndoReport { restored: r.deleted, failed: r.failed + r.kept, errors })
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
}

/// Permanently delete a batch's quarantined files. A copy quarantined with its twin
/// (`quarantine_copy`) is deleted only if the twin is still there with exactly the same bytes.
/// Deleted entries leave the journal's undo list; the batch counts as emptied once nothing is left.
pub fn purge_batch_with(conn: &Connection, batch_id: &str, stop: &crate::StopFlag, progress: crate::ProgressFn) -> Result<PurgeReport> {
    purge_batch_opts(conn, batch_id, false, stop, progress)
}

/// `purge_batch_with`; with `quick` the copy and its twin are compared in parts only
/// (`Comparer::same_sampled`): much less to read, and a difference between the parts is not seen.
pub fn purge_batch_opts(conn: &Connection, batch_id: &str, quick: bool, stop: &crate::StopFlag, progress: crate::ProgressFn) -> Result<PurgeReport> {
    let entries: Vec<(i64, i64, String, Option<i64>, i64)> = {
        let mut st = conn.prepare(
            "SELECT j.id, j.file_id, j.dst_abs, j.new_file_id, COALESCE(f.size, 0) FROM journal j LEFT JOIN files f ON f.id = j.file_id
             WHERE j.batch_id = ? AND j.op = 'quarantine' AND j.undone = 0",
        )?;
        let rows = st.query_map([batch_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    let mut rep = PurgeReport { quick, ..Default::default() };
    let total = entries.len() as u64;
    let bytes_total: u64 = entries.iter().map(|e| e.4.max(0) as u64).sum();
    let (mut bytes_done, started) = (0u64, std::time::Instant::now());
    let stage = if quick { "verify_quick" } else { "verify" };
    let mut cmp = Comparer::new();
    for (i, (jid, file_id, dst, twin, indexed_size)) in entries.into_iter().enumerate() {
        if stop.is_stopped() {
            rep.cancelled = true;
            break;
        }
        progress(crate::Progress::new("purge", stage, i as u64, total, dst.clone()).with_bytes(bytes_done, bytes_total, started.elapsed()));
        bytes_done += indexed_size.max(0) as u64;
        // Debug builds only: a slow disk on demand (`MO_SLOW_COMPARE_MS` per file), to see the
        // progress a real run of hours shows.
        #[cfg(debug_assertions)]
        if let Some(ms) = std::env::var("MO_SLOW_COMPARE_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        let p = PathBuf::from(&dst);
        if let Some(twin) = twin {
            let twin_path = db::get_file(conn, twin)?.filter(|f| !f.quarantined && !f.missing && f.online).map(|f| f.abs_path());
            let same = match &twin_path {
                Some(tp) if p.exists() => if quick { cmp.same_sampled(&p, tp) } else { cmp.same(&p, tp) }.unwrap_or(false),
                _ => false,
            };
            if !same {
                rep.kept += 1;
                if rep.errors.len() < 200 {
                    rep.errors.push(format!("{}: 원본과 내용이 같은지 확인하지 못해 남겨 둠", dst));
                }
                continue;
            }
        }
        let size = fs::metadata(&p).map(|m| m.len() as i64).unwrap_or(0);
        let r = if p.exists() { fs::remove_file(&p).map_err(anyhow::Error::from) } else { Ok(()) };
        match r {
            Ok(()) => {
                conn.execute("DELETE FROM files WHERE id = ? AND quarantined = 1", [file_id])?;
                conn.execute("UPDATE journal SET op = 'purged', undone = 1 WHERE id = ?", [jid])?;
                rep.deleted += 1;
                rep.bytes += size;
                if let Some(qroot) = p.ancestors().find(|a| a.file_name().map(|n| n == batch_id).unwrap_or(false)) {
                    prune_empty_dirs(p.parent().unwrap_or(qroot), qroot.parent().unwrap_or(qroot));
                }
            }
            Err(e) => {
                rep.failed += 1;
                rep.errors.push(e.to_string());
            }
        }
    }
    if !rep.cancelled {
        progress(crate::Progress::new("purge", stage, total, total, "").with_bytes(bytes_total, bytes_total, started.elapsed()));
    }
    let left: i64 = conn.query_row("SELECT COUNT(*) FROM journal WHERE batch_id = ? AND undone = 0", [batch_id], |r| r.get(0))?;
    if left == 0 {
        conn.execute("UPDATE batches SET state = 'purged' WHERE id = ?", [batch_id])?;
    }
    Ok(rep)
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
        match quarantine(&conn, &batch, &f) {
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

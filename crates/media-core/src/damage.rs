//! "Does this file look damaged?" — for photos and videos that came out of data recovery, or
//! were cut short by a copy that failed: files whose name, size and beginning are fine and
//! whose content is not.
//!
//! What is checked is the file's structure, not how it decodes: decoders are forgiving on
//! purpose (a JPEG cut in half opens as a picture with a grey lower half and no error).
//!  * JPEG: the segments are walked to the end of the image. No end marker → cut short.
//!    A stretch of 4 KiB of zeros inside the compressed image data — which compression never
//!    produces — → a hole (clusters the recovery tool could not read).
//!  * PNG: the chunks are walked to `IEND`.
//!  * MP4 / MOV / HEIC (ISO-BMFF): the top-level boxes are walked (headers only, nothing else
//!    is read). A box that runs past the end of the file → cut short; no `moov` (video) or
//!    `meta` (HEIC) → the index is missing and the file cannot be played or shown.
//!
//! The result is a hint for the user to look ("깨진 것 같음"), never a reason to delete by itself.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::{db, volumes, Db, Progress, ProgressFn, StopFlag};

/// Why a file looks damaged, as shown to the user. `None`: nothing wrong was found (or the
/// format is not one this looks into).
pub fn check_file(path: &Path) -> Result<Option<String>> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut head = [0u8; 16];
    let n = read_some(&mut f, &mut head)?;
    if len == 0 {
        return Ok(Some("빈 파일 (0바이트)".into()));
    }
    let head = &head[..n];
    if head.starts_with(&[0xFF, 0xD8]) {
        // Photos are read whole (they are a few MB); anything huge under a .jpg name is skipped.
        if len > 512 << 20 {
            return Ok(None);
        }
        let mut data = Vec::with_capacity(len as usize);
        f.seek(SeekFrom::Start(0))?;
        f.read_to_end(&mut data)?;
        return Ok(jpeg(&data));
    }
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        return png(&mut f, len);
    }
    if n >= 8 && is_box_type(&head[4..8]) && KNOWN_FIRST.contains(&&head[4..8]) {
        return bmff(&mut f, len);
    }
    if head.iter().all(|b| *b == 0) {
        // Not any format: a file a recovery tool made from clusters it could not read.
        let mut more = vec![0u8; 64 * 1024];
        let m = read_some(&mut f, &mut more)?;
        if more[..m].iter().all(|b| *b == 0) {
            return Ok(Some("내용이 0으로 채워져 있음".into()));
        }
    }
    Ok(None)
}

fn read_some(f: &mut File, buf: &mut [u8]) -> Result<usize> {
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

const BLOCK: usize = 4096;
const CUT: &str = "파일이 중간에서 끝남 (끝 표시가 없음)";

/// A JPEG from its first byte: `None` if it runs to the end of its image without holes.
pub fn jpeg(data: &[u8]) -> Option<String> {
    let len = data.len();
    let mut i = 2;
    // Compressed image data seen so far: [start, end) of each scan.
    let mut scans: Vec<(usize, usize)> = Vec::new();
    loop {
        if i + 2 > len {
            return Some(with_holes(CUT, data, &scans, len));
        }
        if data[i] != 0xFF {
            // Not at a marker where one must be: the structure is broken from here.
            return Some(if scans.is_empty() { "JPEG 구조가 깨짐 (머리말)".into() } else { with_holes(CUT, data, &scans, len) });
        }
        let m = data[i + 1];
        match m {
            0xFF => i += 1, // fill byte
            0xD9 => {
                // End of image.
                if scans.is_empty() {
                    return Some("이미지 데이터가 없음".into());
                }
                let holes = zero_blocks(data, &scans);
                return (holes > 0).then(|| holes_text(holes));
            }
            0xD8 | 0x01 | 0xD0..=0xD7 => i += 2,
            _ => {
                if i + 4 > len {
                    return Some(with_holes(CUT, data, &scans, len));
                }
                let l = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
                if l < 2 {
                    return Some("JPEG 구조가 깨짐".into());
                }
                i += 2 + l;
                if m != 0xDA {
                    continue;
                }
                // A scan: compressed data up to the next real marker (FF 00 is data, FF D0–D7
                // are restart marks inside it).
                let start = i.min(len);
                let mut j = start;
                let next = loop {
                    match data[j..].iter().position(|b| *b == 0xFF) {
                        None => break None,
                        Some(k) => {
                            j += k;
                            match data.get(j + 1) {
                                None => break None,
                                Some(0x00) | Some(0xD0..=0xD7) => j += 2,
                                Some(0xFF) => j += 1,
                                Some(_) => break Some(j),
                            }
                        }
                    }
                };
                match next {
                    Some(at) => {
                        scans.push((start, at));
                        i = at;
                    }
                    None => {
                        scans.push((start, len));
                        return Some(with_holes(CUT, data, &scans, len));
                    }
                }
            }
        }
    }
}

/// Whole 4 KiB blocks of zeros inside the scans.
fn zero_blocks(data: &[u8], scans: &[(usize, usize)]) -> usize {
    scans.iter().map(|(a, b)| data[*a..*b].chunks_exact(BLOCK).filter(|c| c.iter().all(|x| *x == 0)).count()).sum()
}

fn holes_text(blocks: usize) -> String {
    format!("내용 중간에 빈 구간이 있음 ({} KB)", blocks * BLOCK / 1024)
}

fn with_holes(what: &str, data: &[u8], scans: &[(usize, usize)], _len: usize) -> String {
    match zero_blocks(data, scans) {
        0 => what.to_string(),
        n => format!("{what} · {}", holes_text(n)),
    }
}

fn png(f: &mut File, len: u64) -> Result<Option<String>> {
    let mut pos = 8u64;
    let mut h = [0u8; 8];
    loop {
        if pos + 12 > len {
            return Ok(Some(CUT.into()));
        }
        f.seek(SeekFrom::Start(pos))?;
        if read_some(f, &mut h)? < 8 {
            return Ok(Some(CUT.into()));
        }
        let size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        if !is_box_type(&h[4..8]) {
            return Ok(Some("PNG 구조가 깨짐".into()));
        }
        if pos + 12 + size > len {
            return Ok(Some(CUT.into()));
        }
        if &h[4..8] == b"IEND" {
            return Ok(None);
        }
        pos += 12 + size;
    }
}

/// Box types a file of this family starts with.
const KNOWN_FIRST: &[&[u8]] = &[b"ftyp", b"moov", b"mdat", b"wide", b"free", b"skip", b"pnot", b"styp"];

fn is_box_type(t: &[u8]) -> bool {
    t.len() == 4 && t.iter().all(|c| c.is_ascii_alphanumeric() || *c == b' ' || *c == b'_' || *c == 0xA9)
}

fn bmff(f: &mut File, len: u64) -> Result<Option<String>> {
    let (mut pos, mut moov, mut meta, mut mdat, mut still) = (0u64, false, false, false, false);
    let mut h = [0u8; 16];
    while pos + 8 <= len {
        f.seek(SeekFrom::Start(pos))?;
        let got = read_some(f, &mut h)?;
        if got < 8 {
            break;
        }
        let typ = [h[4], h[5], h[6], h[7]];
        // Padding or something appended after the last box: not ours to judge.
        if !is_box_type(&typ) {
            break;
        }
        let mut size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        let mut header = 8;
        if size == 1 {
            if got < 16 {
                return Ok(Some(CUT.into()));
            }
            size = u64::from_be_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]]);
            header = 16;
        } else if size == 0 {
            size = len - pos; // "to the end of the file"
        }
        if size < header {
            return Ok(Some("파일 구조가 깨짐".into()));
        }
        match &typ {
            b"moov" => moov = true,
            b"meta" => meta = true,
            b"mdat" => mdat = true,
            b"ftyp" => {
                // Still-image brands (HEIC / AVIF): the index is `meta`, there is no `moov`.
                let mut brand = [0u8; 4];
                f.seek(SeekFrom::Start(pos + 8))?;
                if read_some(f, &mut brand)? == 4 {
                    still = matches!(&brand, b"heic" | b"heix" | b"heim" | b"heis" | b"mif1" | b"msf1" | b"avif" | b"avis" | b"hevc" | b"hevx");
                }
            }
            _ => {}
        }
        if pos + size > len {
            return Ok(Some(format!("{CUT} — {:.0}% 지점", len as f64 / (pos + size) as f64 * 100.0)));
        }
        pos += size;
    }
    if still && !meta {
        return Ok(Some("사진 색인(meta)이 없음 — 열 수 없음".into()));
    }
    if !still && mdat && !moov {
        return Ok(Some("영상 색인(moov)이 없음 — 재생할 수 없음".into()));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------------------------
// In the index
// ---------------------------------------------------------------------------------------------

/// Record the result for a file: a "damaged" suggestion (shown in 「정리」) or none, and that it
/// was looked at.
pub fn record(conn: &Connection, file_id: i64, found: Option<&str>) -> Result<()> {
    match found {
        Some(why) => conn.execute(
            "INSERT INTO suggestions(file_id, kind, detail) VALUES (?1, 'damaged', ?2)
             ON CONFLICT(file_id, kind) DO UPDATE SET detail = excluded.detail",
            params![file_id, why],
        )?,
        None => conn.execute("DELETE FROM suggestions WHERE file_id = ? AND kind = 'damaged'", [file_id])?,
    };
    conn.execute("UPDATE analysis SET damage_checked = 1 WHERE file_id = ?", [file_id])?;
    Ok(())
}

/// Analysed files on connected drives that were not looked at yet (libraries analysed before
/// this check existed).
pub fn unchecked(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM analysis a JOIN files f ON f.id = a.file_id JOIN volumes v ON v.id = f.volume_id
         WHERE a.damage_checked = 0 AND f.missing = 0 AND f.quarantined = 0 AND v.online = 1",
        [],
        |r| r.get(0),
    )?)
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DamageReport {
    pub checked: u64,
    pub damaged: u64,
    pub unreadable: u64,
    pub cancelled: bool,
}

/// Look at every analysed file not looked at yet, in folder order (one drive read at a time).
/// Photos are read whole, videos only at their box headers. Can be stopped and run again.
pub fn run(db: &Db, stop: &StopFlag, progress: ProgressFn) -> Result<DamageReport> {
    let conn = db.conn()?;
    volumes::refresh(&conn)?;
    let rows: Vec<(i64, String, String, i64)> = {
        let mut st = conn.prepare(
            "SELECT f.id, v.mount, f.rel_path, f.size FROM analysis a JOIN files f ON f.id = a.file_id JOIN volumes v ON v.id = f.volume_id
             WHERE a.damage_checked = 0 AND f.missing = 0 AND f.quarantined = 0 AND v.online = 1
             ORDER BY f.volume_id, f.rel_path",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    let mut rep = DamageReport::default();
    let total = rows.len() as u64;
    let bytes_total: u64 = rows.iter().map(|r| r.3.max(0) as u64).sum();
    let (mut bytes_done, started) = (0u64, std::time::Instant::now());
    conn.execute_batch("BEGIN")?;
    for (i, (id, mount, rel, size)) in rows.into_iter().enumerate() {
        if stop.is_stopped() {
            rep.cancelled = true;
            break;
        }
        if i % 25 == 0 {
            progress(Progress::new("check:damage", "check", i as u64, total, rel.clone()).with_bytes(bytes_done, bytes_total, started.elapsed()));
        }
        if i > 0 && i % 500 == 0 {
            conn.execute_batch("COMMIT; BEGIN")?;
        }
        bytes_done += size.max(0) as u64;
        let path = volumes::join_rel(Path::new(&mount), &rel);
        match crate::guarded("손상 검사", || check_file(&path)) {
            Ok(found) => {
                record(&conn, id, found.as_deref())?;
                rep.checked += 1;
                rep.damaged += found.is_some() as u64;
            }
            // Not readable right now (in use, the drive hiccuped): left for the next run.
            Err(_) => rep.unreadable += 1,
        }
    }
    conn.execute_batch("COMMIT")?;
    progress(Progress::new("check:damage", "done", total, total, ""));
    let _ = db::schema_version(&conn);
    Ok(rep)
}

// ---------------------------------------------------------------------------------------------
// Looking at them, and deleting the ones the user chose
// ---------------------------------------------------------------------------------------------

/// Photo formats the app always reads itself: one of these that could not be opened is broken
/// (a RAW or HEIC file that could not be opened may only be of a kind the app does not know).
const ALWAYS_READABLE: &str = "('jpg', 'jpeg', 'png', 'gif', 'bmp', 'webp', 'tif', 'tiff')";
const NOT_OPENED: &str = "열 수 없음: 사진을 읽는 중에 오류가 났습니다";

/// Photos the analysis could not open join the damaged ones (they were only counted before).
/// A photo that opens at a later try leaves the list again (`record`).
pub fn adopt_unreadable(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        &format!(
            "INSERT OR IGNORE INTO suggestions(file_id, kind, detail)
             SELECT a.file_id, 'damaged', ?1 FROM analysis a JOIN files f ON f.id = a.file_id
             WHERE a.tries >= 1 AND f.kind = 'photo' AND lower(f.ext) IN {ALWAYS_READABLE} AND f.missing = 0 AND f.quarantined = 0"
        ),
        [NOT_OPENED],
    )?)
}

#[derive(Clone, Debug, Serialize)]
pub struct DamagedFile {
    pub id: i64,
    pub name: String,
    /// The folder it is in, as a path.
    pub folder: String,
    pub size: i64,
    pub kind: String,
    /// What is wrong with it.
    pub why: String,
    pub taken_at: Option<i64>,
    /// Other files with the same fingerprint (size and beginning) that are not on this list:
    /// most likely good copies of the same photo.
    pub other_copies: i64,
    pub online: bool,
    /// ♥, marked as original or in a folder to keep: not deleted from here.
    pub protected: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DamagedList {
    pub files: Vec<DamagedFile>,
    pub total: i64,
    pub bytes: i64,
}

const LISTED: &str = "f.missing = 0 AND f.quarantined = 0 AND NOT EXISTS (SELECT 1 FROM dismissals d WHERE d.file_id = f.id AND d.kind = 'damaged')";

/// The files that look damaged or could not be opened, in folder order.
pub fn list(conn: &Connection, offset: i64, limit: i64) -> Result<DamagedList> {
    list_in(conn, None, offset, limit)
}

/// `list` for the files inside the work scope.
pub fn list_in(conn: &Connection, scope: Option<&crate::db::Scope>, offset: i64, limit: i64) -> Result<DamagedList> {
    adopt_unreadable(conn)?;
    let inside = crate::db::and_scope(scope, "f");
    let (total, bytes): (i64, i64) = conn.query_row(
        &format!("SELECT COUNT(*), COALESCE(SUM(f.size), 0) FROM files f JOIN suggestions s ON s.file_id = f.id AND s.kind = 'damaged' WHERE {LISTED}{inside}"),
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut st = conn.prepare(&format!(
        "SELECT f.id, f.name, v.mount, f.rel_path, f.size, f.kind, s.detail, f.taken_at, v.online,
                (SELECT COUNT(*) FROM files g WHERE COALESCE(g.prehash, g.hash) = COALESCE(f.prehash, f.hash) AND g.id != f.id AND g.missing = 0 AND g.quarantined = 0
                   AND NOT EXISTS (SELECT 1 FROM suggestions x WHERE x.file_id = g.id AND x.kind = 'damaged'))
         FROM files f JOIN suggestions s ON s.file_id = f.id AND s.kind = 'damaged' JOIN volumes v ON v.id = f.volume_id
         WHERE {LISTED}{inside} ORDER BY f.volume_id, f.rel_path LIMIT ?1 OFFSET ?2"
    ))?;
    let mut files: Vec<DamagedFile> = st
        .query_map(params![limit, offset], |r| {
            let (mount, rel): (String, String) = (r.get(2)?, r.get(3)?);
            let abs = volumes::join_rel(Path::new(&mount), &rel);
            Ok(DamagedFile {
                id: r.get(0)?,
                name: r.get(1)?,
                folder: abs.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
                size: r.get(4)?,
                kind: r.get(5)?,
                why: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                taken_at: r.get(7)?,
                online: r.get(8)?,
                other_copies: r.get(9)?,
                protected: false,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let ids: Vec<i64> = files.iter().map(|f| f.id).collect();
    let kept = protected_of(conn, &ids)?;
    for f in &mut files {
        f.protected = kept.contains(&f.id);
    }
    Ok(DamagedList { files, total, bytes })
}

/// ♥ / original / keep-folder files among `ids` (being the last copy does not protect a damaged file).
fn protected_of(conn: &Connection, ids: &[i64]) -> Result<std::collections::HashSet<i64>> {
    Ok(crate::protect::check(conn, ids)?.into_iter().filter(|b| b.reason != "last_copy").map(|b| b.id).collect())
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DamagedDeleteReport {
    pub batch_id: String,
    pub deleted: u64,
    pub bytes: i64,
    /// ♥ / original / keep-folder files: left alone.
    pub kept_protected: u64,
    /// Not on the list (any more), or on a drive that is not connected.
    pub skipped: u64,
    pub failed: u64,
    pub errors: Vec<String>,
    pub cancelled: bool,
}

/// Delete for good the damaged files the user chose after looking at them. Only files on the
/// list are taken; protected ones stay. They are not compared with their good copy first (they
/// differ from it by nature), and nothing about them is remembered: a good copy of the same
/// photo is still welcome in the library.
pub fn delete(db: &Db, ids: &[i64], stop: &StopFlag, progress: ProgressFn) -> Result<DamagedDeleteReport> {
    delete_in(db, ids, None, stop, progress)
}

/// `delete`, only for files inside the work scope.
pub fn delete_in(db: &Db, ids: &[i64], scope: Option<&crate::db::Scope>, stop: &StopFlag, progress: ProgressFn) -> Result<DamagedDeleteReport> {
    let conn = db.conn()?;
    let mut rep = DamagedDeleteReport::default();
    let inside = crate::db::and_scope(scope, "f");
    let mut st = conn.prepare(&format!("SELECT 1 FROM files f JOIN suggestions s ON s.file_id = f.id AND s.kind = 'damaged' WHERE f.id = ? AND {LISTED}{inside}"))?;
    let mut take: Vec<i64> = Vec::new();
    for id in ids {
        if st.exists([id])? {
            take.push(*id);
        } else {
            rep.skipped += 1;
        }
    }
    drop(st);
    let kept = protected_of(&conn, &take)?;
    rep.kept_protected = kept.len() as u64;
    take.retain(|id| !kept.contains(id));
    if take.is_empty() {
        return Ok(rep);
    }
    let d = crate::journal::delete_files_as(db, &take, true, false, Some("깨진 파일"), stop, progress)?;
    rep.batch_id = d.batch_id.clone();
    rep.skipped += d.skipped_offline;
    rep.failed = d.failed;
    rep.errors = d.errors;
    rep.cancelled = d.cancelled;
    if !d.batch_id.is_empty() && !d.cancelled {
        let p = crate::journal::purge_batch_with(&conn, &d.batch_id, stop, progress)?;
        rep.deleted = p.deleted;
        rep.bytes = p.bytes;
        rep.failed += p.failed + p.kept;
        rep.errors.extend(p.errors);
        rep.cancelled = p.cancelled;
    }
    Ok(rep)
}

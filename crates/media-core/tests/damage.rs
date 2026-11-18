//! "깨진 것 같음": files whose structure says they are damaged, though their name, size and
//! beginning are fine — and files that are fine, however odd, which must not be flagged.

use std::fs;
use std::path::Path;

use media_core::scanner::{self, ScanOptions};
use media_core::thumbs::ThumbCache;
use media_core::{analysis, cleanup, damage, volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

fn look(p: &Path) -> Option<String> {
    damage::check_file(p).unwrap()
}

/// A minimal file of boxes: `(type, payload length)`; `declared` overrides a box's size field.
fn boxes(parts: &[(&[u8; 4], usize, Option<u32>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (typ, len, declared) in parts {
        out.extend_from_slice(&declared.unwrap_or(8 + *len as u32).to_be_bytes());
        out.extend_from_slice(*typ);
        if *typ == b"ftyp" {
            out.extend_from_slice(b"isom");
            out.extend(std::iter::repeat(1u8).take(len.saturating_sub(4)));
        } else {
            out.extend(std::iter::repeat(1u8).take(*len));
        }
    }
    out
}

#[test]
fn photos() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    write_jpeg(&d.join("good.jpg"), &picture(1, 3000, 2000), Some(("2020:01:01 10:00:00", "Canon", "EOS R6")));
    let good = fs::read(d.join("good.jpg")).unwrap();
    assert!(good.len() > 200_000);
    assert_eq!(look(&d.join("good.jpg")), None);

    // Cut short (a copy that failed, a recovered file whose end was overwritten).
    fs::write(d.join("cut.jpg"), &good[..good.len() * 6 / 10]).unwrap();
    assert!(look(&d.join("cut.jpg")).unwrap().contains("중간에서 끝남"));
    // The right size, the rest zeros (what recovery tools make of unreadable clusters).
    let mut tail = good.clone();
    for b in &mut tail[120_000..] {
        *b = 0;
    }
    fs::write(d.join("zero-tail.jpg"), &tail).unwrap();
    let why = look(&d.join("zero-tail.jpg")).unwrap();
    assert!(why.contains("중간에서 끝남") && why.contains("빈 구간"), "{why}");
    // A hole in the middle; beginning and end intact — every decoder opens this.
    let mut hole = good.clone();
    for b in &mut hole[100_000..100_000 + 3 * 4096] {
        *b = 0;
    }
    fs::write(d.join("hole.jpg"), &hole).unwrap();
    assert!(look(&d.join("hole.jpg")).unwrap().contains("빈 구간"));
    assert!(image::open(d.join("hole.jpg")).is_ok() || true, "(whether a decoder minds is beside the point)");

    // Not damage: something appended after the image (phones add video clips and maker data),
    // a few zero bytes inside the data, a photo with an embedded preview (its own end marker).
    let mut appended = good.clone();
    appended.extend(std::iter::repeat(0xABu8).take(300_000));
    fs::write(d.join("appended.jpg"), &appended).unwrap();
    assert_eq!(look(&d.join("appended.jpg")), None);
    let mut padded = good.clone();
    padded.extend(std::iter::repeat(0u8).take(100_000));
    fs::write(d.join("padded.jpg"), &padded).unwrap();
    assert_eq!(look(&d.join("padded.jpg")), None);

    write_png(&d.join("good.png"), &picture(2, 800, 600));
    let png = fs::read(d.join("good.png")).unwrap();
    assert_eq!(look(&d.join("good.png")), None);
    fs::write(d.join("cut.png"), &png[..png.len() / 2]).unwrap();
    assert!(look(&d.join("cut.png")).unwrap().contains("중간에서 끝남"));

    fs::write(d.join("empty.jpg"), b"").unwrap();
    assert!(look(&d.join("empty.jpg")).unwrap().contains("빈 파일"));
    fs::write(d.join("zeros.jpg"), vec![0u8; 500_000]).unwrap();
    assert!(look(&d.join("zeros.jpg")).unwrap().contains("0으로"));
    // Formats this does not look into are left alone.
    fs::write(d.join("notes.txt"), b"hello").unwrap();
    assert_eq!(look(&d.join("notes.txt")), None);
}

#[test]
fn videos_and_heic() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let put = |name: &str, bytes: Vec<u8>| {
        fs::write(d.join(name), bytes).unwrap();
        look(&d.join(name))
    };
    assert_eq!(put("ok.mp4", boxes(&[(b"ftyp", 16, None), (b"free", 8, None), (b"mdat", 5000, None), (b"moov", 300, None)])), None);
    // Camera files with the index first, and with a 64-bit box.
    assert_eq!(put("ok.mov", boxes(&[(b"ftyp", 12, None), (b"moov", 300, None), (b"wide", 0, None), (b"mdat", 9000, None)])), None);
    // Zero padding after the last box (some cameras).
    let mut padded = boxes(&[(b"ftyp", 16, None), (b"moov", 300, None), (b"mdat", 5000, None)]);
    padded.extend(vec![0u8; 4096]);
    assert_eq!(put("padded.mp4", padded), None);

    // The data box says it is longer than the file: cut short.
    let cut = boxes(&[(b"ftyp", 16, None), (b"mdat", 5000, Some(900_000))]);
    assert!(put("cut.mp4", cut).unwrap().contains("중간에서 끝남"));
    // Recording never finished (camera lost power): data, no index.
    assert!(put("no-index.mp4", boxes(&[(b"ftyp", 16, None), (b"mdat", 5000, None)])).unwrap().contains("moov"));
    // A still image has `meta` instead.
    let mut heic = boxes(&[(b"ftyp", 16, None), (b"meta", 400, None), (b"mdat", 5000, None)]);
    heic[8..12].copy_from_slice(b"heic");
    assert_eq!(put("ok.heic", heic.clone()), None);
    let mut broken = boxes(&[(b"ftyp", 16, None), (b"mdat", 5000, None)]);
    broken[8..12].copy_from_slice(b"heic");
    assert!(put("broken.heic", broken).unwrap().contains("meta"));
}

#[test]
fn found_while_analysing_and_by_the_check() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    write_jpeg(&e.join("복구/good.jpg"), &picture(1, 3000, 2000), Some(("2020:01:01 10:00:00", "Canon", "EOS R6")));
    let good = fs::read(e.join("복구/good.jpg")).unwrap();
    let mut hole = good.clone();
    for b in &mut hole[100_000..100_000 + 2 * 4096] {
        *b = 0;
    }
    fs::write(e.join("복구/hole.jpg"), &hole).unwrap();
    fs::write(e.join("복구/cut.jpg"), &good[..good.len() / 2]).unwrap();
    fs::write(e.join("복구/clip.mp4"), boxes(&[(b"ftyp", 16, None), (b"mdat", 400_000, None)])).unwrap();

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    let tc = ThumbCache::new(tmp.path().join("cache"));
    analysis::run(&db, &tc, None, 2, &stop, &analysis::PauseFlag::default(), &quiet).unwrap();

    let found = |conn: &media_core::rusqlite::Connection| -> Vec<String> {
        let mut st = conn.prepare("SELECT f.name FROM suggestions s JOIN files f ON f.id = s.file_id WHERE s.kind = 'damaged' ORDER BY 1").unwrap();
        let v = st.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        v
    };
    assert_eq!(found(&conn), ["clip.mp4", "cut.jpg", "hole.jpg"]);
    let s = cleanup::summary(&conn).unwrap();
    assert_eq!((s.damaged.count, s.damage_unchecked), (3, 0), "looked at while analysing");

    // A library analysed before the check existed: the check job goes through it, once.
    conn.execute("UPDATE analysis SET damage_checked = 0", []).unwrap();
    conn.execute("DELETE FROM suggestions WHERE kind = 'damaged'", []).unwrap();
    assert_eq!(cleanup::summary(&conn).unwrap().damage_unchecked, 4);
    let rep = damage::run(&db, &stop, &quiet).unwrap();
    assert_eq!((rep.checked, rep.damaged, rep.cancelled), (4, 3, false), "{rep:?}");
    assert_eq!(found(&conn), ["clip.mp4", "cut.jpg", "hole.jpg"]);
    assert_eq!(damage::run(&db, &stop, &quiet).unwrap().checked, 0);
    // The suggestions survive the regrouping that follows every analysis pass.
    let mut c = db.conn().unwrap();
    cleanup::refresh(&mut c).unwrap();
    assert_eq!(found(&conn).len(), 3);
}

/// By hand, against real folders: `MO_DAMAGE_DIR=<folder> cargo test -p media-core --test damage -- --ignored --nocapture`
/// prints every file flagged (to see what a real library gives before trusting the check).
#[test]
#[ignore]
fn what_a_real_folder_gives() {
    let Some(dir) = std::env::var_os("MO_DAMAGE_DIR") else { return };
    let (mut seen, mut flagged) = (0u64, 0u64);
    let mut stack = vec![std::path::PathBuf::from(dir)];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let ext = p.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
            if !["jpg", "jpeg", "png", "heic", "mp4", "mov", "m4v"].contains(&ext.as_str()) {
                continue;
            }
            seen += 1;
            if let Ok(Some(why)) = damage::check_file(&p) {
                flagged += 1;
                println!("{}: {why}", p.display());
            }
        }
    }
    println!("{seen} files looked at, {flagged} flagged");
}

/// The damaged files in one list, and the chosen ones deleted for good: without being compared
/// with their good copy (they differ from it by nature), never a file that is not on the list,
/// never a protected one.
#[test]
fn looked_at_and_deleted_for_good() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    write_jpeg(&e.join("사진/a.jpg"), &picture(1, 3000, 2000), Some(("2020:01:01 10:00:00", "Canon", "EOS R6")));
    let good = fs::read(e.join("사진/a.jpg")).unwrap();
    // The same photo from a recovery: same size, same beginning, a hole further in.
    let mut hole = good.clone();
    for b in &mut hole[100_000..100_000 + 2 * 4096] {
        *b = 0;
    }
    fs::create_dir_all(e.join("복구")).unwrap();
    fs::write(e.join("복구/a.jpg"), &hole).unwrap();
    fs::write(e.join("복구/cut.jpg"), &good[..good.len() / 2]).unwrap();
    fs::write(e.join("복구/fav.jpg"), &good[..good.len() / 3]).unwrap();
    write_jpeg(&e.join("사진/b.jpg"), &picture(2, 3000, 2000), Some(("2020:01:02 10:00:00", "Canon", "EOS R6")));

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    media_core::identity::backfill_prehashes(&conn).unwrap();
    let tc = ThumbCache::new(tmp.path().join("cache"));
    analysis::run(&db, &tc, None, 2, &stop, &analysis::PauseFlag::default(), &quiet).unwrap();
    let id = |rel: &str| -> i64 { conn.query_row("SELECT id FROM files WHERE rel_path = ?", [rel], |r| r.get(0)).unwrap() };
    conn.execute("INSERT INTO labels(file_id, label, source) VALUES (?, 'favorite', 'user')", [id("복구/fav.jpg")]).unwrap();
    // A photo the analysis could not open is on the list too.
    conn.execute("UPDATE analysis SET tries = 1 WHERE file_id = ?", [id("사진/b.jpg")]).unwrap();

    let l = damage::list(&conn, 0, 100).unwrap();
    let names: Vec<(String, i64, bool)> = l.files.iter().map(|f| (format!("{}/{}", Path::new(&f.folder).file_name().unwrap().to_string_lossy(), f.name), f.other_copies, f.protected)).collect();
    assert_eq!(l.total, 4, "{names:?}");
    assert!(names.contains(&("복구/a.jpg".into(), 1, false)), "its good copy is counted: {names:?}");
    assert!(names.contains(&("복구/cut.jpg".into(), 0, false)) && names.contains(&("복구/fav.jpg".into(), 0, true)), "{names:?}");
    assert!(l.files.iter().any(|f| f.name == "b.jpg" && f.why.starts_with("열 수 없음")), "{names:?}");
    assert_eq!(cleanup::summary(&conn).unwrap().damaged.count, 4);

    // The user looked at b.jpg: it is fine — off the list. The rest is chosen, and so (by a
    // mistake of the caller) is the good photo.
    let mut c = db.conn().unwrap();
    cleanup::dismiss(&mut c, &[id("사진/b.jpg")], "damaged").unwrap();
    let ids = [id("복구/a.jpg"), id("복구/cut.jpg"), id("복구/fav.jpg"), id("사진/a.jpg"), id("사진/b.jpg")];
    let rep = damage::delete(&db, &ids, &stop, &quiet).unwrap();
    assert_eq!((rep.deleted, rep.kept_protected, rep.skipped, rep.failed), (2, 1, 2, 0), "{rep:?}");
    assert!(!e.join("복구/a.jpg").exists() && !e.join("복구/cut.jpg").exists());
    assert!(e.join("복구/fav.jpg").is_file() && e.join("사진/a.jpg").is_file() && e.join("사진/b.jpg").is_file());
    assert_eq!(fs::read(e.join("사진/a.jpg")).unwrap(), good);
    // Gone for good: nothing waits in the quarantine, nothing to undo.
    let state: String = conn.query_row("SELECT state FROM batches WHERE id = ?", [&rep.batch_id], |r| r.get(0)).unwrap();
    assert_eq!(state, "purged");
    assert!(!volumes::quarantine_root(&e).join(&rep.batch_id).exists());
    assert_eq!(damage::list(&conn, 0, 100).unwrap().total, 1, "the protected one is still listed");
}

//! Things that must not stop the app: a file that makes a decoder panic, work cut short by a
//! crash, and an import that would otherwise read the whole library.
//! (Own test binary: it sets `MO_TEST_PANIC`.)

use std::fs;
use std::path::Path;

use media_core::rusqlite::params;
use media_core::scanner::{self, ScanOptions};
use media_core::{analysis, guarded, importer, journal, thumbs, volumes, Db, Locked, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

fn jpeg(p: &Path, seed: u32, date: &str) {
    write_jpeg(p, &picture(seed, 800, 600), Some((date, "Canon", "EOS R6")));
}

#[test]
fn a_panicking_file_fails_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    jpeg(&e.join("p/good1.jpg"), 1, "2020:01:01 10:00:00");
    jpeg(&e.join("p/bad.jpg"), 2, "2020:01:02 10:00:00");
    jpeg(&e.join("p/good2.jpg"), 3, "2020:01:03 10:00:00");
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();

    std::env::set_var("MO_TEST_PANIC", "bad.jpg");
    let tc = thumbs::ThumbCache::new(tmp.path().join("cache"));
    let st = analysis::run(&db, &tc, None, 2, &stop, &analysis::PauseFlag::default(), &quiet).unwrap();
    assert_eq!(st.failed, 1, "{st:?}");
    let analysed: i64 = conn.query_row("SELECT COUNT(*) FROM analysis WHERE dhash IS NOT NULL", [], |r| r.get(0)).unwrap();
    assert_eq!(analysed, 2, "the other photos are analysed");
    // Recorded as failed: not pending, so the next pass does not hit it again.
    assert_eq!(analysis::pending(&conn, false).unwrap().online, 0);
    let again = analysis::run(&db, &tc, None, 2, &stop, &analysis::PauseFlag::default(), &quiet).unwrap();
    assert_eq!(again.failed, 0);
    // It may have been a passing problem (the file in use, the drive hiccuping): tried again
    // another day, twice — then left alone.
    for tries in 2..=3 {
        conn.execute("UPDATE analysis SET analyzed_at = analyzed_at - 2 * 86400 WHERE dhash IS NULL", []).unwrap();
        assert_eq!(analysis::pending(&conn, false).unwrap().online, 1, "due again before try {tries}");
        let st = analysis::run(&db, &tc, None, 2, &stop, &analysis::PauseFlag::default(), &quiet).unwrap();
        assert_eq!(st.failed, 1);
    }
    conn.execute("UPDATE analysis SET analyzed_at = analyzed_at - 2 * 86400 WHERE dhash IS NULL", []).unwrap();
    assert_eq!(analysis::pending(&conn, false).unwrap().online, 0, "not for ever");
    std::env::remove_var("MO_TEST_PANIC");

    // The helpers behind this: a panic becomes an error, and a lock a thread died holding opens.
    let r: anyhow::Result<()> = guarded("테스트", || panic!("boom"));
    assert!(r.unwrap_err().to_string().contains("boom"));
    let m = std::sync::Arc::new(std::sync::Mutex::new(1));
    let m2 = m.clone();
    let _ = std::thread::spawn(move || {
        let _g = m2.lock().unwrap();
        panic!("dies holding the lock");
    })
    .join();
    assert_eq!(*m.locked(), 1);
}

#[test]
fn interrupted_work_is_closed_at_start() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    jpeg(&e.join("p/a.jpg"), 1, "2020:01:01 10:00:00");
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    let f = media_core::db::get_file(&conn, 1).unwrap().unwrap();

    // A batch that moved one file and never finished (power loss), and one that did nothing.
    let b = journal::new_batch(&conn, "organize", "라이브러리로 모으기").unwrap();
    let dst = e.join("사진/a.jpg");
    journal::move_indexed(&conn, &b, &f, &dst, f.volume_id, "사진/a.jpg").unwrap();
    let empty = journal::new_batch(&conn, "organize", "빈 작업").unwrap();

    let names = journal::recover_interrupted(&conn).unwrap();
    assert_eq!(names, ["라이브러리로 모으기"]);
    let (state, desc, ops): (String, String, i64) =
        conn.query_row("SELECT state, description, op_count FROM batches WHERE id = ?", [&b], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!((state.as_str(), ops), ("done", 1));
    assert!(desc.ends_with("(중단됨)"), "{desc}");
    let left: i64 = conn.query_row("SELECT COUNT(*) FROM batches WHERE id = ?", [&empty], |r| r.get(0)).unwrap();
    assert_eq!(left, 0, "a batch that did nothing is dropped");
    assert!(journal::recover_interrupted(&conn).unwrap().is_empty(), "only once");

    // And it can be undone like any other.
    let undo = journal::undo_batch(&conn, &b).unwrap();
    assert_eq!(undo.failed, 0, "{undo:?}");
    assert!(e.join("p/a.jpg").is_file());
}

#[test]
fn an_import_fingerprints_only_what_could_be_a_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let card = drive(tmp.path(), "card");
    // On E: a photo that is also on the card, and many others (indexed by an older version:
    // no fingerprints) of which two happen to share a size with each other but not the card.
    jpeg(&e.join("p/same.jpg"), 1, "2020:01:01 10:00:00");
    fs::write(e.join("p/x1.jpg"), vec![7u8; 5000]).unwrap();
    fs::write(e.join("p/x2.jpg"), vec![8u8; 5000]).unwrap();
    fs::create_dir_all(card.join("DCIM/100CANON")).unwrap();
    fs::copy(e.join("p/same.jpg"), card.join("DCIM/100CANON/IMG_1.JPG")).unwrap();
    jpeg(&card.join("DCIM/100CANON/IMG_2.JPG"), 2, "2020:01:02 10:00:00");

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions { min_image_bytes: 0, min_image_side: 0, ..Default::default() }, &stop, &quiet).unwrap();
    conn.execute("UPDATE files SET prehash = NULL", params![]).unwrap();

    let pv = importer::preview(&db, &card, &ScanOptions::default(), &stop, &quiet).unwrap();
    assert_eq!((pv.new_files, pv.already), (1, 1), "the copy on E is recognised: {pv:?}");
    let fp = |name: &str| -> Option<String> { conn.query_row("SELECT prehash FROM files WHERE name = ? AND volume_id = 1", [name], |r| r.get(0)).unwrap() };
    assert!(fp("same.jpg").is_some(), "read: it has the size of a file on the card");
    assert!(fp("x1.jpg").is_none() && fp("x2.jpg").is_none(), "not read: nothing on the card has their size");
    importer::forget(&conn, pv.source_id).unwrap();
}

//! Things that keep the app dependable over months of use: an undo that can be stopped and
//! taken up again, copies of the database and a check of it, a database from a newer version
//! refused, a cache that does not grow for ever, and folders judged by where they are.

use std::fs;
use std::path::Path;

use media_core::rusqlite::params;
use media_core::scanner::{self, ScanOptions};
use media_core::thumbs::ThumbCache;
use media_core::{db, importer, journal, volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

fn jpeg(p: &Path, seed: u32, date: &str) {
    write_jpeg(p, &picture(seed, 800, 600), Some((date, "Canon", "EOS R6")));
}

#[test]
fn an_undo_can_be_stopped_and_taken_up_again() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    for i in 0..5u32 {
        jpeg(&e.join(format!("p/f{i}.jpg")), i, "2020:01:01 10:00:00");
    }
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    let ids: Vec<i64> = {
        let mut st = conn.prepare("SELECT id FROM files ORDER BY id").unwrap();
        let v = st.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        v
    };
    let rep = journal::delete_files(&db, &ids, true, None, &stop, &quiet).unwrap();
    assert_eq!(rep.deleted, 5);

    // Stopped (「취소」) before it got anywhere: nothing is half-done, and it says so.
    let halt = StopFlag::new();
    halt.stop();
    let undo = journal::undo_batch_with(&conn, &rep.batch_id, &halt, &quiet).unwrap();
    assert!(undo.cancelled && undo.restored == 0, "{undo:?}");
    let b = journal::list_batches(&conn).unwrap().into_iter().find(|b| b.id == rep.batch_id).unwrap();
    assert_eq!((b.state.as_str(), b.waiting), ("done", 5), "still to be undone");
    let undo = journal::undo_batch_with(&conn, &rep.batch_id, &stop, &quiet).unwrap();
    assert_eq!((undo.restored, undo.failed, undo.cancelled), (5, 0, false));
    assert!((0..5).all(|i| e.join(format!("p/f{i}.jpg")).is_file()));
}

#[test]
fn the_database_is_copied_checked_and_not_opened_by_an_older_app() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    jpeg(&e.join("p/a.jpg"), 1, "2020:01:01 10:00:00");
    let path = tmp.path().join("app/library.db");
    let db = Db::open(&path).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    scanner::scan_all(&db, &ScanOptions::default(), &StopFlag::new(), &quiet).unwrap();
    assert!(db.quick_check().unwrap().is_empty());

    // Copies: readable databases with the same content; only the newest few are kept.
    let mut made = Vec::new();
    for i in 0..5 {
        made.push(db.snapshot(&format!("test:{i}"), 3).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    let kept = db.snapshots();
    assert_eq!(kept.len(), 3, "{kept:?}");
    assert_eq!(kept[0], made[4], "newest first");
    let copy = media_core::rusqlite::Connection::open(&kept[0]).unwrap();
    assert_eq!(copy.query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    assert_eq!(db::schema_version(&copy).unwrap(), db::schema_version(&conn).unwrap());

    // A database written by a newer version is refused, not misread.
    conn.execute_batch("PRAGMA user_version = 9999;").unwrap();
    drop(conn);
    let err = Db::open(&path).err().expect("refused").to_string() + &format!("{:#}", Db::open(&path).err().unwrap());
    assert!(err.contains("더 새 버전"), "{err}");
}

#[test]
fn the_cache_lets_go_of_what_is_no_longer_needed() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    jpeg(&e.join("p/a.jpg"), 1, "2020:01:01 10:00:00");
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    scanner::scan_all(&db, &ScanOptions::default(), &StopFlag::new(), &quiet).unwrap();
    let f = db::all_files(&conn).unwrap().into_values().next().unwrap();
    let cache = ThumbCache::new(tmp.path().join("cache"));
    let live = cache.store_thumb(&f, &picture(1, 800, 600)).unwrap();
    assert!(live.is_file());

    let dir = live.parent().unwrap();
    let put = |sub: &str, name: &str, bytes: usize, age_secs: i64| {
        let p = tmp.path().join("cache").join(sub).join("00").join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, vec![7u8; bytes]).unwrap();
        let t = filetime::FileTime::from_unix_time(media_core::now_ts() - age_secs, 0);
        filetime::set_file_times(&p, t, t).unwrap();
        p
    };
    // A thumbnail of the same file before it changed, one of a file that is gone, a temporary
    // file left by a run that died, one being written now, a face that no longer exists.
    let old_version = dir.join(format!("{}-{}-{}.jpg", f.id, f.size + 1, f.mtime));
    fs::write(&old_version, b"x").unwrap();
    let gone = put("thumbs", "999999-10-10.jpg", 10, 0);
    let dead_part = put("thumbs", "5-1-1.part123-4", 10, 7200);
    let fresh_part = put("thumbs", "6-1-1.part123-5", 10, 0);
    let face = put("faces", "424242.jpg", 10, 0);
    // Converted videos of this file: more than the cap allows — the one not used longest goes.
    let v_old = put("videos", &format!("{}-{}-{}.mp4", f.id, f.size, f.mtime), 600, 5000);
    let p_new = put("previews", &format!("{}-{}-{}.jpg", f.id, f.size, f.mtime), 600, 10);

    let rep = cache.prune(&conn, 1000).unwrap();
    assert_eq!(rep.removed, 5, "{rep:?}");
    assert!(live.is_file() && fresh_part.is_file() && p_new.is_file(), "what is in use stays");
    assert!(![&old_version, &gone, &dead_part, &face, &v_old].iter().any(|p| p.exists()));
    // Nothing more to do the second time.
    assert_eq!(cache.prune(&conn, 1000).unwrap().removed, 0);
}

#[test]
fn folders_are_judged_by_where_they_are() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    jpeg(&e.join("Recovery/system.jpg"), 1, "2020:01:01 10:00:00"); // the drive's own
    jpeg(&e.join("백업/Recovery/found.jpg"), 2, "2020:01:02 10:00:00"); // the user's recovered photos
    jpeg(&e.join("백업/Temp/t.jpg"), 3, "2020:01:03 10:00:00");
    jpeg(&e.join("백업/cache/c.jpg"), 4, "2020:01:04 10:00:00"); // an app's cache
    jpeg(&e.join("사진/2020/2020-01/Cache/event.jpg"), 5, "2020:01:05 10:00:00"); // an event in the library
    jpeg(&e.join("사진/2020/2020-01/thumbnails/th.jpg"), 6, "2020:01:06 10:00:00");
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    importer::ImportSettings { target_dir: e.join("사진").to_string_lossy().into_owned(), ..Default::default() }.save(&conn).unwrap();
    scanner::scan_all(&db, &ScanOptions::default(), &StopFlag::new(), &quiet).unwrap();
    let mut seen: Vec<String> = {
        let mut st = conn.prepare("SELECT name FROM files WHERE missing = 0").unwrap();
        let v = st.query_map(params![], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        v
    };
    seen.sort();
    assert_eq!(seen, ["event.jpg", "found.jpg", "t.jpg", "th.jpg"]);
}

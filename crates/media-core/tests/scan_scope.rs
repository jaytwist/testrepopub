//! Checking only what changed: one folder (today's SD dump), or one drive.

use std::fs;

use media_core::scanner::{self, ScanOptions};
use media_core::{volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

#[test]
fn only_the_folder_or_drive_asked_for() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let c = drive(tmp.path(), "diskC");
    write_jpeg(&e.join("사진/2026-10/a.jpg"), &picture(1, 800, 600), Some(("2026:10:01 10:00:00", "Canon", "EOS")));
    write_jpeg(&e.join("사진/2026-10/b.jpg"), &picture(2, 800, 600), Some(("2026:10:02 10:00:00", "Canon", "EOS")));
    write_jpeg(&c.join("Pictures/c.jpg"), &picture(3, 800, 600), Some(("2026:09:01 10:00:00", "Apple", "iPhone")));
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    let se = volumes::add_source(&conn, &e).unwrap();
    let sc = volumes::add_source(&conn, &c).unwrap();
    let stop = StopFlag::new();
    let opts = ScanOptions::default();
    scanner::scan_all(&db, &opts, &stop, &quiet).unwrap();

    // Today's dump goes into a new folder; meanwhile a file elsewhere disappears.
    write_jpeg(&e.join("사진/2026-11/new.jpg"), &picture(4, 800, 600), Some(("2026:11:08 09:00:00", "Canon", "EOS")));
    fs::remove_file(e.join("사진/2026-10/b.jpg")).unwrap();

    // "Only this folder": found by path, the new photo is added, nothing outside is touched.
    let (sid, sub) = scanner::locate(&conn, &e.join("사진").join("2026-11")).unwrap().unwrap();
    assert_eq!((sid, sub.as_str()), (se, "사진/2026-11"));
    let st = scanner::scan_scope(&db, sid, Some(&sub), &opts, &stop, &quiet).unwrap();
    assert_eq!((st.seen, st.added, st.missing), (1, 1, 0), "{st:?}");
    let missing: i64 = conn.query_row("SELECT COUNT(*) FROM files WHERE missing = 1", [], |r| r.get(0)).unwrap();
    assert_eq!(missing, 0, "b.jpg is outside the folder checked");
    assert!(scanner::locate(&conn, tmp.path()).unwrap().is_none(), "not inside a registered folder");

    // "Only this drive": drive C is not listed again.
    let last_c: i64 = conn.query_row("SELECT last_scan FROM sources WHERE id = ?", [sc], |r| r.get(0)).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let vol_e: i64 = conn.query_row("SELECT volume_id FROM sources WHERE id = ?", [se], |r| r.get(0)).unwrap();
    let stats = scanner::scan_volume(&db, vol_e, &opts, &stop, &quiet).unwrap();
    assert_eq!(stats.len(), 1);
    assert_eq!((stats[0].source_id, stats[0].missing), (se, 1), "b.jpg is noticed on a full check of E");
    let after_c: i64 = conn.query_row("SELECT last_scan FROM sources WHERE id = ?", [sc], |r| r.get(0)).unwrap();
    assert_eq!(last_c, after_c, "drive C was not scanned");
}

//! "한 해씩": the library plan split by year, one year run at a time — copies on the library drive
//! deleted (after a byte check) to make room, then moves, then copies from other drives as far
//! as the room goes. (Own test binary: it sets the free-space environment variables.)

use std::fs;
use std::path::Path;

use media_core::journal;
use media_core::library::LibraryOptions;
use media_core::organizer::{self, ExecOptions, OrganizeOptions};
use media_core::scanner::{self, ScanOptions};
use media_core::{volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

fn jpeg(p: &Path, seed: u32, date: &str) {
    write_jpeg(p, &picture(seed, 800, 600), Some((date, "Canon", "EOS R6")));
}

#[test]
fn year_by_year_with_copies_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let c = drive(tmp.path(), "diskC");

    // 2015: an event, an identical copy elsewhere on E, a ♥ copy, and a "copy" that only looks
    // identical (same size and first 64 KiB, different end).
    jpeg(&e.join("옛날/2015-08 제주도/j1.jpg"), 1, "2015:08:10 10:00:00");
    fs::create_dir_all(e.join("백업")).unwrap();
    fs::copy(e.join("옛날/2015-08 제주도/j1.jpg"), e.join("백업/j1.jpg")).unwrap();
    fs::copy(e.join("옛날/2015-08 제주도/j1.jpg"), e.join("백업/j1 ♥.jpg")).unwrap();
    write_jpeg(&e.join("옛날/2015-08 제주도/j2.jpg"), &picture(2, 3000, 2000), Some(("2015:08:11 10:00:00", "Canon", "EOS R6")));
    let mut fake = fs::read(e.join("옛날/2015-08 제주도/j2.jpg")).unwrap();
    assert!(fake.len() > 70_000, "the fake copy must differ after the first 64 KiB");
    let n = fake.len();
    fake[n - 10] ^= 0xFF;
    fs::write(e.join("백업/j2.jpg"), &fake).unwrap();
    // Another year on E, one before 2010, and a 2015 photo only on C.
    jpeg(&e.join("DCIM/100CANON/y16.jpg"), 3, "2016:03:01 10:00:00");
    jpeg(&e.join("DCIM/100CANON/old.jpg"), 4, "2008:05:01 10:00:00");
    jpeg(&c.join("Pictures/c15.jpg"), 5, "2015:12:24 10:00:00");

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    volumes::add_source(&conn, &c).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    // ♥ on one copy.
    let heart: i64 = conn.query_row("SELECT id FROM files WHERE name = 'j1 ♥.jpg'", [], |r| r.get(0)).unwrap();
    conn.execute("INSERT INTO labels(file_id, label, source) VALUES (?, 'favorite', 'user')", [heart]).unwrap();

    let lib = e.join("사진");
    let opts = OrganizeOptions {
        target_dir: lib.to_string_lossy().into_owned(),
        layout: "library".into(),
        library: LibraryOptions::default(),
        ..Default::default()
    };
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    let years = organizer::year_buckets(&plan);
    let keys: Vec<&str> = years.iter().map(|y| y.key.as_str()).collect();
    assert_eq!(keys, ["before2010", "2015", "2016"], "{years:?}");
    let y15 = &years[1];
    assert_eq!((y15.moves, y15.copies, y15.remove), (2, 1, 2), "the ♥ copy is not counted: {y15:?}");
    let folders = organizer::year_copies(&plan, "2015");
    assert_eq!(folders.len(), 1);
    assert!(folders[0].dir.replace('\\', "/").ends_with("/백업"), "{folders:?}");
    assert_eq!(folders[0].kept_in[0].0, "2015/2015-08/제주도");
    let pairs = organizer::year_copy_pairs(&plan, "2015", &folders[0].dir, 0, 10);
    assert_eq!(pairs.len(), 2);
    assert!(pairs.iter().all(|p| p.kept_dst.replace('\\', "/").contains("2015/2015-08/제주도/")), "{pairs:?}");

    // No room on E for the copy from C: the year still runs; the copy waits.
    std::env::set_var("MO_SPACE_RESERVE", "0");
    std::env::set_var("MO_FREE_SPACE", "1000");
    let part = organizer::subset_year(&plan, "2015");
    let xo = ExecOptions { purge: true, label: Some("2015년".into()), ..Default::default() };
    let rep = organizer::execute_with(&db, &part, &xo, &stop, &quiet).unwrap();
    std::env::remove_var("MO_FREE_SPACE");
    assert_eq!(rep.failed, 0, "{:?}", rep.errors);
    assert_eq!((rep.done, rep.space_skipped), (2, 1), "moved two, the copy from C waits for room");
    assert_eq!(rep.remove_skipped, 0, "the ♥ copy is not even planned");
    assert!(e.join("백업/j1 ♥.jpg").is_file());
    assert_eq!((rep.purged, rep.purge_kept), (1, 1), "{:?}", rep.errors);
    assert!(!e.join("백업/j1.jpg").exists());
    assert!(!walk(&e).iter().any(|p| p.ends_with("/j1.jpg") && p.contains(".media-organizer")), "deleted for good, not in the quarantine");
    assert!(walk(&e).iter().any(|p| p.ends_with("/j2.jpg") && !p.contains("사진/")), "the look-alike stays in the quarantine");
    assert!(lib.join("2015/2015-08/제주도/j1.jpg").is_file());
    // Other years untouched.
    assert!(e.join("DCIM/100CANON/y16.jpg").is_file());
    assert!(e.join("DCIM/100CANON/old.jpg").is_file());

    // Planning again: 2015 only has the copy from C left (and the ♥ copy, kept on purpose).
    let again = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    let y15 = organizer::year_buckets(&again).into_iter().find(|y| y.key == "2015").unwrap();
    assert_eq!((y15.moves, y15.copies), (0, 1), "{y15:?}");
    let rep2 = organizer::execute_with(&db, &organizer::subset_year(&again, "2015"), &xo, &stop, &quiet).unwrap();
    assert_eq!((rep2.done, rep2.space_skipped), (1, 0));
    assert!(lib.join("2015/2015-12/c15.jpg").is_file());
    assert!(c.join("Pictures/c15.jpg").is_file(), "C stays as it is");

    // Organizing is undone on its own, even though copies were deleted.
    let undo = journal::undo_batch(&conn, &rep.batch_id).unwrap();
    assert_eq!(undo.failed, 0, "{undo:?}");
    assert!(e.join("옛날/2015-08 제주도/j1.jpg").is_file());
    // The look-alike left in the quarantine comes back with its own undo.
    let undo = journal::undo_batch(&conn, &rep.copies_batch).unwrap();
    assert_eq!(undo.failed, 0, "{undo:?}");
    assert!(e.join("백업/j2.jpg").is_file());
}

fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out
}

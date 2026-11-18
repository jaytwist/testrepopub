//! The same photo under other names and in other folders shows once in the library, with the
//! number of its other copies; where they all are comes from `identity::family`.

use std::fs;

use media_core::db::{self, FileFilter};
use media_core::scanner::{self, ScanOptions};
use media_core::{identity, journal, volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

#[test]
fn one_tile_for_the_same_photo_and_where_its_copies_are() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, f) = (drive(tmp.path(), "diskE"), drive(tmp.path(), "diskF"));
    let date = Some(("2020:05:01 10:00:00", "Canon", "EOS R6"));
    write_jpeg(&e.join("사진/IMG_0001.jpg"), &picture(1, 1600, 1200), date);
    fs::create_dir_all(e.join("복구/dir7")).unwrap();
    fs::create_dir_all(f.join("백업")).unwrap();
    // The same photo under another name, and on another drive.
    fs::copy(e.join("사진/IMG_0001.jpg"), e.join("복구/dir7/f1234567.jpg")).unwrap();
    fs::copy(e.join("사진/IMG_0001.jpg"), f.join("백업/여행 첫날.jpg")).unwrap();
    // Another photo, alone.
    write_jpeg(&e.join("사진/IMG_0002.jpg"), &picture(2, 1600, 1200), date);

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let mut conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    volumes::add_source(&conn, &f).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    identity::backfill_prehashes(&conn).unwrap();
    identity::refresh_roles(&mut conn).unwrap();

    let stacked = FileFilter { stack_copies: true, ..Default::default() };
    let grid = db::query_grid(&conn, &stacked, 0, 100).unwrap();
    let mut seen: Vec<(String, i64)> = grid.iter().map(|g| (g.name.clone(), g.same)).collect();
    seen.sort();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(seen.iter().map(|s| s.1).collect::<Vec<_>>(), [2, 0], "+2 on the photo that has copies: {seen:?}");
    // Counts follow what is shown.
    let groups = db::group_summary(&conn, &stacked, "month").unwrap();
    assert_eq!(groups.iter().map(|g| g.count).sum::<i64>(), 2);
    // Not stacked: all four. A drive on its own: its files, stacked or not, each with its "+2".
    assert_eq!(db::query_grid(&conn, &FileFilter::default(), 0, 100).unwrap().len(), 4);
    let vol_f: i64 = conn.query_row("SELECT id FROM volumes WHERE mount LIKE '%diskF'", [], |r| r.get(0)).unwrap();
    let on_f = db::query_grid(&conn, &FileFilter { stack_copies: true, volume_id: Some(vol_f), ..Default::default() }, 0, 100).unwrap();
    assert_eq!(on_f.iter().map(|g| (g.name.as_str(), g.same)).collect::<Vec<_>>(), [("여행 첫날.jpg", 2)]);

    // Where they all are: the original first, then the others with what they are.
    let shown = grid.iter().find(|g| g.same == 2).unwrap().id;
    let fam = identity::family(&conn, shown).unwrap().unwrap();
    assert_eq!(fam.kind, "exact");
    assert_eq!(fam.members.len(), 3);
    assert!(fam.members[0].is_self && fam.members[0].role == "original");
    let places: Vec<String> = fam.members.iter().map(|m| m.abs_path.replace('\\', "/")).collect();
    for end in ["사진/IMG_0001.jpg", "복구/dir7/f1234567.jpg", "백업/여행 첫날.jpg"] {
        assert!(places.iter().any(|p| p.ends_with(end)), "{end}: {places:?}");
    }
    assert!(fam.members.iter().any(|m| m.role == "backup") && fam.members.iter().all(|m| !m.damaged));

    // The one shown is deleted: another copy stands for the photo, with one less behind it.
    journal::delete_files(&db, &[shown], false, None, &stop, &quiet).unwrap();
    identity::refresh_roles(&mut conn).unwrap();
    let after = db::query_grid(&conn, &stacked, 0, 100).unwrap();
    let mut left: Vec<i64> = after.iter().map(|g| g.same).collect();
    left.sort();
    assert_eq!(left, [0, 1], "{:?}", after.iter().map(|g| &g.name).collect::<Vec<_>>());
}

//! "Never read a known file twice": files moved, renamed or copied outside the app keep (or
//! reuse) everything already derived from them; nothing is decoded or analysed again.

use std::fs;
use std::path::Path;

use media_core::db::{self, FileFilter};
use media_core::scanner::{self, ScanOptions};
use media_core::{analysis, labels, thumbs, volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

/// `cp -p`: Explorer and most backup tools keep the modification time.
fn copy_keep_time(src: &Path, dst: &Path) {
    fs::create_dir_all(dst.parent().unwrap()).unwrap();
    fs::copy(src, dst).unwrap();
    let t = fs::metadata(src).unwrap().modified().unwrap();
    fs::File::options().write(true).open(dst).unwrap().set_modified(t).unwrap();
}

fn id_of(conn: &rusqlite::Connection, rel: &str) -> Option<i64> {
    conn.query_row("SELECT id FROM files WHERE rel_path = ? AND missing = 0", [rel], |r| r.get(0)).ok()
}

#[test]
fn moved_renamed_and_copied_files_are_not_read_again() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let a = drive(root, "diskA");
    let b = drive(root, "diskB");
    for i in 0..6u32 {
        write_jpeg(&a.join(format!("Trip/IMG_{i}.jpg")), &picture(i, 900, 600), Some((&format!("2023:05:0{} 10:00:00", i + 1), "Apple", "iPhone 13")));
    }
    let db = Db::open(root.join("appdata/library.db")).unwrap();
    let mut conn = db.conn().unwrap();
    volumes::add_source(&conn, &a).unwrap();
    volumes::add_source(&conn, &b).unwrap();
    let stop = StopFlag::new();
    let opts = ScanOptions::default();
    scanner::scan_all(&db, &opts, &stop, &quiet).unwrap();
    let tc = thumbs::ThumbCache::new(root.join("appdata/cache"));
    let st = analysis::run(&db, &tc, None, 2, &stop, &analysis::PauseFlag::default(), &quiet).unwrap();
    assert_eq!(st.processed, 6);

    // The user's own work on two files.
    let img0 = id_of(&conn, "Trip/IMG_0.jpg").unwrap();
    let img1 = id_of(&conn, "Trip/IMG_1.jpg").unwrap();
    labels::add_user_label(&mut conn, &[img0], "가족").unwrap();
    labels::set_taken_at(&mut conn, &[img1], 1_600_000_000).unwrap();

    // Outside the app: rename one file, move the folder, back up three files to another drive,
    // and create a different photo with the same size and time as one of them.
    fs::rename(a.join("Trip/IMG_0.jpg"), a.join("Trip/beach.jpg")).unwrap();
    fs::create_dir_all(a.join("2023")).unwrap();
    fs::rename(a.join("Trip"), a.join("2023/Trip")).unwrap();
    for i in 2..5u32 {
        copy_keep_time(&a.join(format!("2023/Trip/IMG_{i}.jpg")), &b.join(format!("backup/IMG_{i}.jpg")));
    }
    let mut other = fs::read(a.join("2023/Trip/IMG_5.jpg")).unwrap();
    let n = other.len();
    other[n / 2] ^= 0xFF; // same size, different content…
    other[100] ^= 0xFF; // …also within the first 64 KiB, which the content check reads
    fs::write(b.join("backup/IMG_5.jpg"), &other).unwrap();
    let t = fs::metadata(a.join("2023/Trip/IMG_5.jpg")).unwrap().modified().unwrap();
    fs::File::options().write(true).open(b.join("backup/IMG_5.jpg")).unwrap().set_modified(t).unwrap();

    let stats = scanner::scan_all(&db, &opts, &stop, &quiet).unwrap();
    let moved: u64 = stats.iter().map(|s| s.moved).sum();
    let reused: u64 = stats.iter().map(|s| s.reused).sum();
    let added: u64 = stats.iter().map(|s| s.added).sum();
    let missing: u64 = stats.iter().map(|s| s.missing).sum();
    assert_eq!(moved, 6, "rename + folder move recognised: {stats:?}");
    assert_eq!(reused, 3, "backup copies reuse results: {stats:?}");
    assert_eq!(added, 1, "only the genuinely different photo is read: {stats:?}");
    assert_eq!(missing, 0, "{stats:?}");

    // Same rows, user work kept.
    assert_eq!(id_of(&conn, "2023/Trip/beach.jpg"), Some(img0));
    assert_eq!(id_of(&conn, "2023/Trip/IMG_1.jpg"), Some(img1));
    assert!(labels::labels_for(&conn, img0).unwrap().iter().any(|l| l.label == "가족"));
    assert_eq!(db::get_file(&conn, img1).unwrap().unwrap().taken_at, Some(1_600_000_000));

    // Nothing to analyse again except the one new photo; copies carry the analysis.
    assert_eq!(analysis::pending(&conn, false).unwrap().online, 1);
    let copy = id_of(&conn, "backup/IMG_2.jpg").unwrap();
    let f = db::get_file(&conn, copy).unwrap().unwrap();
    assert!(f.clone_of.is_some());
    assert!(tc.has_thumb(&f), "the copy takes over the original's thumbnail");
    let n_analysis: i64 = conn.query_row("SELECT COUNT(*) FROM analysis WHERE file_id = ?", [copy], |r| r.get(0)).unwrap();
    assert_eq!(n_analysis, 1);

    // A later scan with nothing changed reads nothing.
    let again = scanner::scan_all(&db, &opts, &stop, &quiet).unwrap();
    assert!(again.iter().all(|s| s.added + s.updated + s.moved + s.reused == 0), "{again:?}");
    let live = db::query_ids(&conn, &FileFilter::default()).unwrap();
    assert_eq!(live.len(), 10);
}

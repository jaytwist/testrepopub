//! The ways a photo could be lost for good, each closed:
//! * a damaged look-alike (same size, same first 64 KiB) taken for "another copy";
//! * a drive that is not connected — or came back under another letter — when the quarantine
//!   is emptied or a batch undone;
//! * the app closed or killed between moving a file and saving that it did;
//! * forgetting an SD card that is a drive the user registered;
//! * a long name losing its extension; a file replacing another.

use std::fs;
use std::path::{Path, PathBuf};

use media_core::library::LibraryOptions;
use media_core::organizer::{self, Action, OrganizeOptions};
use media_core::rusqlite::params;
use media_core::scanner::{self, ScanOptions};
use media_core::{identity, importer, journal, volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

fn jpeg(p: &Path, seed: u32, date: &str) {
    write_jpeg(p, &picture(seed, 3000, 2000), Some((date, "Canon", "EOS R6")));
}

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n != ".mo-volume") {
                out.push(p);
            }
        }
    }
    out
}

fn id_of(conn: &media_core::rusqlite::Connection, rel: &str) -> i64 {
    conn.query_row("SELECT id FROM files WHERE rel_path = ?", [rel], |r| r.get(0)).unwrap()
}

fn open(tmp: &Path, drives: &[&Path]) -> (Db, StopFlag) {
    let db = Db::open(tmp.join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    for d in drives {
        volumes::add_source(&conn, d).unwrap();
    }
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    identity::backfill_prehashes(&conn).unwrap();
    (db, stop)
}

#[test]
fn a_damaged_look_alike_is_not_another_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    // The good photo, a true copy of it, and a "copy" damaged after the first 64 KiB.
    jpeg(&e.join("사진/a.jpg"), 1, "2019:05:01 10:00:00");
    let good = fs::read(e.join("사진/a.jpg")).unwrap();
    assert!(good.len() > 200_000);
    fs::create_dir_all(e.join("복구")).unwrap();
    let mut bad = good.clone();
    for b in &mut bad[100_000..] {
        *b = 0;
    }
    fs::write(e.join("복구/a.jpg"), &bad).unwrap();
    jpeg(&e.join("사진/b.jpg"), 2, "2019:05:02 10:00:00");
    fs::create_dir_all(e.join("백업")).unwrap();
    fs::copy(e.join("사진/b.jpg"), e.join("백업/b.jpg")).unwrap();

    let (db, stop) = open(tmp.path(), &[&e]);
    let conn = db.conn().unwrap();
    let (a, a_bad, b) = (id_of(&conn, "사진/a.jpg"), id_of(&conn, "복구/a.jpg"), id_of(&conn, "사진/b.jpg"));
    let key = |id: i64| -> String { conn.query_row("SELECT COALESCE(prehash, hash) FROM files WHERE id = ?", [id], |r| r.get(0)).unwrap() };
    assert_eq!(key(a), key(a_bad), "the two look the same to the index");

    // The user deletes the good one and one of the true copies: both are let through ("another
    // copy stays") and go to the quarantine.
    let rep = journal::delete_files(&db, &[a, b], false, None, &stop, &quiet).unwrap();
    assert_eq!((rep.deleted, rep.protected), (2, 0), "{rep:?}");
    // Emptying the quarantine compares first: the true copy goes, the good photo is kept.
    let p = journal::purge_batch_with(&conn, &rep.batch_id, &stop, &quiet).unwrap();
    assert_eq!((p.deleted, p.kept), (1, 1), "{p:?}");
    assert_eq!(p.differing, vec![(a, a_bad)]);
    assert!(files_under(&e).iter().any(|f| fs::read(f).unwrap() == good), "the good photo still exists");
    assert!(e.join("백업/b.jpg").is_file() && !e.join("사진/b.jpg").exists());
    // From now on the two are different files to the index.
    assert_ne!(key(a), key(a_bad));
    // And the good one comes back with an undo.
    let undo = journal::undo_batch(&conn, &rep.batch_id).unwrap();
    assert_eq!((undo.restored, undo.failed), (1, 0), "{undo:?}");
    assert_eq!(fs::read(e.join("사진/a.jpg")).unwrap(), good);
    // The damaged one is no reason to let the good one go any more: with a true copy made of
    // the good photo, deleting both asks to keep one.
    fs::copy(e.join("사진/a.jpg"), e.join("백업/a.jpg")).unwrap();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    identity::backfill_prehashes(&conn).unwrap();
    let a2 = id_of(&conn, "백업/a.jpg");
    let rep = journal::delete_files(&db, &[a, a2], false, None, &stop, &quiet).unwrap();
    assert_eq!((rep.deleted, rep.protected), (1, 1), "{rep:?}");
}

#[test]
fn a_drive_that_is_away_is_not_an_empty_quarantine() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    jpeg(&e.join("p/x.jpg"), 3, "2020:01:01 10:00:00");
    jpeg(&e.join("p/y.jpg"), 4, "2020:01:02 10:00:00");
    let (db, stop) = open(tmp.path(), &[&e]);
    let conn = db.conn().unwrap();
    let x = id_of(&conn, "p/x.jpg");
    let rep = journal::delete_files(&db, &[x], true, None, &stop, &quiet).unwrap();
    assert_eq!(rep.deleted, 1);
    let held: Vec<PathBuf> = files_under(&e).into_iter().filter(|p| p.to_string_lossy().contains("quarantine")).collect();
    assert_eq!(held.len(), 1);

    // The drive is unplugged (here: its folder is somewhere else).
    let away = tmp.path().join("unplugged");
    fs::rename(&e, &away).unwrap();
    let p = journal::purge_batch_with(&conn, &rep.batch_id, &stop, &quiet).unwrap();
    assert_eq!((p.deleted, p.failed), (0, 1), "{p:?}");
    let waiting = journal::list_batches(&conn).unwrap().into_iter().find(|b| b.id == rep.batch_id).unwrap();
    assert_eq!((waiting.state.as_str(), waiting.waiting), ("done", 1), "still to be emptied");
    assert!(conn.query_row("SELECT quarantined FROM files WHERE id = ?", [x], |r| r.get::<_, i64>(0)).unwrap() == 1, "the index still knows it");
    let undo = journal::undo_batch(&conn, &rep.batch_id).unwrap();
    assert_eq!((undo.restored, undo.failed), (0, 1), "nothing to put back while the drive is away");

    // It comes back under another letter (another path, the same volume).
    let f = tmp.path().join("diskF");
    fs::rename(&away, &f).unwrap();
    let f = volumes::canonical(&f).unwrap();
    conn.execute("UPDATE volumes SET uid = ?, mount = ?", params![format!("mount:{}", f.display()), f.to_string_lossy()]).unwrap();
    let undo = journal::undo_batch(&conn, &rep.batch_id).unwrap();
    assert_eq!((undo.restored, undo.failed), (1, 0), "{undo:?}");
    assert!(f.join("p/x.jpg").is_file(), "back where it was, on the drive as it is now");
    // And deleting for good works there too.
    let rep = journal::delete_files(&db, &[x], true, None, &stop, &quiet).unwrap();
    let f2 = tmp.path().join("diskG");
    fs::rename(&f, &f2).unwrap();
    let f2 = volumes::canonical(&f2).unwrap();
    conn.execute("UPDATE volumes SET uid = ?, mount = ?", params![format!("mount:{}", f2.display()), f2.to_string_lossy()]).unwrap();
    let p = journal::purge_batch_with(&conn, &rep.batch_id, &stop, &quiet).unwrap();
    assert_eq!((p.deleted, p.failed), (1, 0), "{p:?}");
    assert!(!files_under(&f2).iter().any(|p| p.ends_with("x.jpg")), "deleted from the quarantine, not left in it");
    assert!(f2.join("p/y.jpg").is_file());
}

#[test]
fn moves_made_before_a_crash_can_still_be_undone() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    for i in 0..6u32 {
        jpeg(&e.join(format!("DCIM/IMG_{i}.jpg")), 10 + i, &format!("2021:0{}:10 10:00:00", i + 1));
    }
    let (db, stop) = open(tmp.path(), &[&e]);
    let conn = db.conn().unwrap();
    let opts = OrganizeOptions { target_dir: e.join("사진").to_string_lossy().into_owned(), layout: "library".into(), library: LibraryOptions::default(), ..Default::default() };
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    let moves: Vec<_> = plan.items.iter().filter(|i| i.action == Action::Move).collect();
    assert_eq!(moves.len(), 6);

    // What the organizer does, cut off after three files were moved and none of it saved.
    fs::create_dir_all(e.join("사진")).unwrap();
    let lib_vol: i64 = conn.query_row("SELECT id FROM volumes", [], |r| r.get(0)).unwrap();
    let batch = journal::new_batch(&conn, "organize", "라이브러리로 모으기").unwrap();
    let planned: Vec<journal::PlannedMove> = moves
        .iter()
        .map(|i| journal::PlannedMove { file_id: i.file_id, src: &i.src, dst: &i.dst, dst_volume: lib_vol, dst_rel: volumes::rel_of(&e, Path::new(&i.dst)).unwrap() })
        .collect();
    journal::plan_moves(&conn, &batch, &planned).unwrap();
    for i in &moves[..3] {
        fs::create_dir_all(Path::new(&i.dst).parent().unwrap()).unwrap();
        fs::rename(&i.src, &i.dst).unwrap();
    }

    // Next start.
    let names = journal::recover_interrupted(&conn).unwrap();
    assert_eq!(names.len(), 1);
    let b = journal::list_batches(&conn).unwrap().into_iter().find(|b| b.id == batch).unwrap();
    assert!(b.description.ends_with("(중단됨)") && b.state == "done" && b.op_count == 3, "{b:?}");
    for i in &moves[..3] {
        let rel: String = conn.query_row("SELECT rel_path FROM files WHERE id = ?", [i.file_id], |r| r.get(0)).unwrap();
        assert!(rel.starts_with("사진/"), "the index knows where it is now: {rel}");
    }
    let left: i64 = conn.query_row("SELECT COUNT(*) FROM journal WHERE undone = 2", [], |r| r.get(0)).unwrap();
    assert_eq!(left, 0, "moves that were not made are forgotten");
    // The interrupted batch is undone like any other.
    let undo = journal::undo_batch(&conn, &batch).unwrap();
    assert_eq!((undo.restored, undo.failed), (3, 0), "{undo:?}");
    for i in &moves {
        assert!(Path::new(&i.src).is_file(), "{}", i.src);
    }
    // And a real run records each move once (the planned row, completed).
    let rep = organizer::execute(&db, &plan, &stop, &quiet).unwrap();
    assert_eq!((rep.done, rep.failed), (6, 0), "{:?}", rep.errors);
    let (rows, pending): (i64, i64) = conn.query_row("SELECT COUNT(*), SUM(undone = 2) FROM journal WHERE batch_id = ? AND op = 'move'", [&rep.batch_id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((rows, pending), (6, 0));
    let undo = journal::undo_batch(&conn, &rep.batch_id).unwrap();
    assert_eq!((undo.restored, undo.failed), (6, 0), "{undo:?}");
}

#[test]
fn a_file_left_in_the_quarantine_by_a_crash_goes_back() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    jpeg(&e.join("p/one.jpg"), 20, "2020:01:01 10:00:00");
    jpeg(&e.join("p/two.jpg"), 21, "2020:01:02 10:00:00");
    let (db, _stop) = open(tmp.path(), &[&e]);
    let conn = db.conn().unwrap();
    let (vol, one) = (conn.query_row("SELECT id FROM volumes", [], |r| r.get::<_, i64>(0)).unwrap(), id_of(&conn, "p/one.jpg"));
    // A delete that got as far as moving the file — and one that also marked it — before dying.
    let batch = journal::new_batch(&conn, "delete", "선택한 2개 삭제").unwrap();
    let q = volumes::quarantine_root(&e).join(&batch).join(format!("v{vol}/p"));
    fs::create_dir_all(&q).unwrap();
    fs::rename(e.join("p/one.jpg"), q.join("one.jpg")).unwrap();
    fs::rename(e.join("p/two.jpg"), q.join("two.jpg")).unwrap();
    conn.execute("UPDATE files SET quarantined = 1 WHERE id = ?", [one]).unwrap();

    journal::recover_interrupted(&conn).unwrap();
    assert!(e.join("p/one.jpg").is_file() && e.join("p/two.jpg").is_file(), "both are back where they were");
    let hidden: i64 = conn.query_row("SELECT COUNT(*) FROM files WHERE quarantined = 1 OR missing = 1", [], |r| r.get(0)).unwrap();
    assert_eq!(hidden, 0);
    assert!(!volumes::quarantine_root(&e).join(&batch).exists(), "nothing left behind");
    assert!(journal::list_batches(&conn).unwrap().iter().all(|b| b.id != batch), "an entry that did nothing is dropped");
}

#[test]
fn forgetting_a_card_leaves_a_registered_drive_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let usb = drive(tmp.path(), "usb");
    let card = drive(tmp.path(), "card");
    jpeg(&e.join("p/a.jpg"), 30, "2020:01:01 10:00:00");
    jpeg(&usb.join("여행/u1.jpg"), 31, "2020:02:01 10:00:00");
    jpeg(&usb.join("여행/u2.jpg"), 32, "2020:02:02 10:00:00");
    jpeg(&usb.join("기타/u3.jpg"), 33, "2020:02:03 10:00:00");
    jpeg(&card.join("DCIM/c1.jpg"), 34, "2020:03:01 10:00:00");
    // The user registered the whole of E, and one folder of the USB drive.
    let (db, stop) = open(tmp.path(), &[&e, &usb.join("여행")]);
    let conn = db.conn().unwrap();
    let u1 = id_of(&conn, "여행/u1.jpg");
    conn.execute("INSERT INTO labels(file_id, label, source) VALUES (?, 'favorite', 'user')", [u1]).unwrap();
    let settings = importer::ImportSettings { target_dir: e.join("사진").to_string_lossy().into_owned(), ..Default::default() };
    let rows = || -> i64 { conn.query_row("SELECT COUNT(*) FROM files WHERE missing = 0", [], |r| r.get(0)).unwrap() };
    let before = rows();

    // "Import" from the USB drive as if it were a card: its new photos are copied …
    let rep = importer::run(&db, &usb, &settings, &ScanOptions::default(), &stop, &quiet).unwrap();
    assert_eq!((rep.copied, rep.failed), (3, 0), "{rep:?}");
    // … and the folder the user registered is still indexed, with its ♥.
    let usb_rows: Vec<String> = {
        let mut st = conn.prepare("SELECT f.rel_path FROM files f JOIN volumes v ON v.id = f.volume_id WHERE v.mount LIKE '%usb' ORDER BY 1").unwrap();
        let rows = st.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        rows
    };
    assert_eq!(usb_rows, ["여행/u1.jpg", "여행/u2.jpg"], "the user's folder stays; the rest of the drive is forgotten");
    let hearts: i64 = conn.query_row("SELECT COUNT(*) FROM labels WHERE file_id = ? AND label = 'favorite'", [u1], |r| r.get(0)).unwrap();
    assert_eq!(hearts, 1);
    let sources: i64 = conn.query_row("SELECT COUNT(*) FROM sources WHERE temp = 0", [], |r| r.get(0)).unwrap();
    assert!(sources >= 2 && conn.query_row("SELECT COUNT(*) FROM sources WHERE temp = 1", [], |r| r.get::<_, i64>(0)).unwrap() == 0);
    assert_eq!(rows(), before + 3);

    // Importing from a registered drive itself: nothing of it is forgotten.
    let rep = importer::run(&db, &usb.join("여행"), &settings, &ScanOptions::default(), &stop, &quiet).unwrap();
    assert_eq!(rep.copied, 0);
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM labels WHERE file_id = ? AND label = 'favorite'", [u1], |r| r.get::<_, i64>(0)).unwrap(), 1);
    // A real card leaves no trace.
    let rep = importer::run(&db, &card, &settings, &ScanOptions::default(), &stop, &quiet).unwrap();
    assert_eq!(rep.copied, 1);
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM volumes WHERE mount LIKE '%card'", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    assert!(card.join("DCIM/c1.jpg").is_file());
}

#[test]
fn nothing_is_replaced_and_no_name_loses_its_extension() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a.jpg"), tmp.path().join("b.jpg"));
    fs::write(&a, b"one").unwrap();
    fs::write(&b, b"two").unwrap();
    assert_eq!(journal::rename_noclobber(&a, &b).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
    assert!(journal::move_file(&a, &b).is_err());
    assert!(journal::copy_verified(&a, &b).is_err());
    assert_eq!((fs::read(&a).unwrap(), fs::read(&b).unwrap()), (b"one".to_vec(), b"two".to_vec()));
    // A copy leaves no temporary file behind, also when it fails; a stale one is replaced.
    fs::write(tmp.path().join(".c.jpg.mo-partial"), b"half").unwrap();
    journal::copy_verified(&a, &tmp.path().join("c.jpg")).unwrap();
    assert!(journal::copy_verified(&tmp.path().join("nope.jpg"), &tmp.path().join("d.jpg")).is_err());
    let names: Vec<String> = fs::read_dir(tmp.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(!names.iter().any(|n| n.contains("mo-partial")), "{names:?}");
    assert_eq!(fs::read(tmp.path().join("c.jpg")).unwrap(), b"one");

    let long = format!("{}.JPG", "가족 여행 사진 ".repeat(30));
    let cut = organizer::sanitize(&long);
    assert!(cut.chars().count() <= 120 && cut.ends_with(".JPG"), "{cut}");
    let cut = organizer::sanitize(&format!("{}. .mp4", "x".repeat(200)));
    assert!(cut.ends_with(".mp4") && !cut.contains(". .") || cut.chars().count() <= 120, "{cut}");
    assert_eq!(organizer::sanitize("IMG_0001.jpg"), "IMG_0001.jpg");
    let no_ext = organizer::sanitize(&"y".repeat(300));
    assert_eq!(no_ext.chars().count(), 120);
}

//! Emptying the whole quarantine in one go: every batch, one progress, going on after a stop —
//! and reading no file that is already known: with both full hashes in the index nothing is
//! read, unless a file changed since its hash was taken.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use media_core::rusqlite::{params, Connection};
use media_core::scanner::{self, ScanOptions};
use media_core::{identity, journal, volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

fn jpeg(p: &Path, seed: u32) {
    write_jpeg(p, &picture(seed, 3000, 2000), Some(("2019:05:01 10:00:00", "Canon", "EOS R6")));
}

fn id_of(conn: &Connection, rel: &str) -> i64 {
    conn.query_row("SELECT id FROM files WHERE rel_path = ?", [rel], |r| r.get(0)).unwrap()
}

fn state(conn: &Connection, batch: &str) -> String {
    conn.query_row("SELECT state FROM batches WHERE id = ?", [batch], |r| r.get(0)).unwrap()
}

/// Drive E with `n` photos in `사진`, each with a true copy in `백업`; scanned.
fn world(tmp: &Path, n: u32) -> (std::path::PathBuf, Db, StopFlag) {
    let e = drive(tmp, "diskE");
    fs::create_dir_all(e.join("백업")).unwrap();
    for i in 0..n {
        jpeg(&e.join(format!("사진/p{i}.jpg")), 100 + i);
        fs::copy(e.join(format!("사진/p{i}.jpg")), e.join(format!("백업/p{i}.jpg"))).unwrap();
    }
    (e, Db::open(tmp.join("app/library.db")).unwrap(), StopFlag::new())
}

fn scan(db: &Db, e: &Path, stop: &StopFlag) {
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, e).unwrap();
    scanner::scan_all(db, &ScanOptions::default(), stop, &quiet).unwrap();
    identity::backfill_prehashes(&conn).unwrap();
}

fn delete(db: &Db, conn: &Connection, rels: &[&str], stop: &StopFlag) -> String {
    let ids: Vec<i64> = rels.iter().map(|r| id_of(conn, r)).collect();
    let rep = journal::delete_files(db, &ids, false, None, stop, &quiet).unwrap();
    assert_eq!(rep.deleted as usize, rels.len(), "{rep:?}");
    rep.batch_id
}

#[test]
fn every_batch_in_one_go() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, db, stop) = world(tmp.path(), 6);
    // A photo whose "other copy" is damaged after the first 64 KiB.
    jpeg(&e.join("사진/d.jpg"), 50);
    let mut bad = fs::read(e.join("사진/d.jpg")).unwrap();
    for b in &mut bad[100_000..] {
        *b = 0;
    }
    fs::create_dir_all(e.join("복구")).unwrap();
    fs::write(e.join("복구/d.jpg"), &bad).unwrap();
    scan(&db, &e, &stop);
    let conn = db.conn().unwrap();

    let b1 = delete(&db, &conn, &["백업/p0.jpg", "백업/p1.jpg"], &stop);
    let b2 = delete(&db, &conn, &["백업/p2.jpg", "사진/d.jpg", "백업/p3.jpg"], &stop);
    let b3 = delete(&db, &conn, &["백업/p4.jpg", "백업/p5.jpg"], &stop);

    // One progress over all three batches: it never goes back, and ends at the total.
    let (last, back) = (AtomicU64::new(0), AtomicU64::new(0));
    let rep = journal::purge_all(&conn, false, &stop, &|p: Progress| {
        assert_eq!(p.total, 7);
        if p.done < last.swap(p.done, Ordering::Relaxed) {
            back.fetch_add(1, Ordering::Relaxed);
        }
    })
    .unwrap();
    assert_eq!(back.load(Ordering::Relaxed), 0, "the progress went back");
    assert_eq!(last.load(Ordering::Relaxed), 7);
    assert_eq!((rep.batches, rep.deleted, rep.kept, rep.failed, rep.cancelled), (3, 6, 1, 0, false), "{rep:?}");
    assert!(rep.bytes > 6 * 200_000);

    // The good photo is still there (in the quarantine, to be put back); the copies are gone
    // and so are their folders in the quarantine.
    assert_eq!((state(&conn, &b1), state(&conn, &b2), state(&conn, &b3)), ("purged".into(), "done".into(), "purged".into()));
    let q = volumes::quarantine_root(&e);
    assert!(!q.join(&b1).exists() && !q.join(&b3).exists(), "emptied batches leave no folders");
    let undo = journal::undo_batch(&conn, &b2).unwrap();
    assert_eq!((undo.restored, undo.failed), (1, 0), "{undo:?}");
    assert!(e.join("사진/d.jpg").is_file());
    for i in 0..6 {
        assert!(e.join(format!("사진/p{i}.jpg")).is_file() && !e.join(format!("백업/p{i}.jpg")).exists());
    }
    // The originals that were read to the end have their full hash now: a further copy of
    // them is checked by reading that copy alone.
    let hashed: i64 = conn.query_row("SELECT COUNT(*) FROM files WHERE rel_path LIKE '사진/p%' AND hash IS NOT NULL", [], |r| r.get(0)).unwrap();
    assert_eq!(hashed, 6);
    // Nothing left: doing it again does nothing.
    let again = journal::purge_all(&conn, false, &stop, &quiet).unwrap();
    assert_eq!((again.batches, again.deleted), (0, 0), "{again:?}");
}

#[test]
fn stopped_and_gone_on_with() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, db, stop) = world(tmp.path(), 6);
    scan(&db, &e, &stop);
    let conn = db.conn().unwrap();
    delete(&db, &conn, &["백업/p0.jpg", "백업/p1.jpg", "백업/p2.jpg"], &stop);
    delete(&db, &conn, &["백업/p3.jpg", "백업/p4.jpg", "백업/p5.jpg"], &stop);

    // Stopped while the fifth file is looked at (in the second batch).
    let halt = StopFlag::new();
    let first = journal::purge_all(&conn, false, &halt, &|p: Progress| {
        if p.done == 4 {
            halt.stop();
        }
    })
    .unwrap();
    assert!(first.cancelled);
    assert_eq!(first.deleted, 5, "{first:?}");
    let waiting: i64 = conn.query_row("SELECT COUNT(*) FROM journal WHERE op = 'quarantine' AND undone = 0", [], |r| r.get(0)).unwrap();
    assert_eq!(waiting, 1, "what was deleted is recorded, the rest waits");

    let rest = journal::purge_all_progress(&conn, false, &stop, &quiet).unwrap();
    assert_eq!((rest.restored, rest.failed, rest.cancelled), (1, 0, false), "{rest:?}");
    let states: Vec<String> = conn.prepare("SELECT state FROM batches").unwrap().query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
    assert_eq!(states, vec!["purged", "purged"]);
}

#[test]
fn what_is_known_is_not_read_again() {
    let tmp = tempfile::tempdir().unwrap();
    let (e, db, stop) = world(tmp.path(), 3);
    scan(&db, &e, &stop);
    let conn = db.conn().unwrap();
    // Pairs 0 and 1: both full hashes are in the index. Pair 2: only the original's.
    for i in 0..3 {
        let h = journal::hash_file(&e.join(format!("사진/p{i}.jpg"))).unwrap();
        conn.execute("UPDATE files SET hash = ?1 WHERE rel_path = ?2 OR (?3 AND rel_path = ?4)", params![h, format!("사진/p{i}.jpg"), i < 2, format!("백업/p{i}.jpg")]).unwrap();
    }
    let batch = delete(&db, &conn, &["백업/p0.jpg", "백업/p1.jpg", "백업/p2.jpg"], &stop);

    // Original 1 changes after its hash was taken (same size, another time, other bytes): the
    // hash in the index says nothing about it any more.
    let p1 = e.join("사진/p1.jpg");
    let mut bytes = fs::read(&p1).unwrap();
    let n = bytes.len();
    bytes[n - 10] ^= 0xFF;
    fs::write(&p1, &bytes).unwrap();
    let f = fs::File::options().write(true).open(&p1).unwrap();
    f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5)).unwrap();
    drop(f);

    let rep = journal::purge_batch_with(&conn, &batch, &stop, &quiet).unwrap();
    assert_eq!((rep.deleted, rep.kept, rep.known), (2, 1, 1), "{rep:?}");
    assert_eq!(rep.differing.len(), 1);
    assert!(volumes::quarantine_root(&e).join(&batch).exists(), "the copy of the changed original stays");
    assert_eq!(state(&conn, &batch), "done");
}

#[test]
fn files_on_two_drives_are_read_at_the_same_time_and_compared_right() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a.bin"), tmp.path().join("b.bin"));
    let data: Vec<u8> = (0..5000usize).map(|i| (i as u8).wrapping_mul(31)).collect();
    fs::write(&a, &data).unwrap();
    let mut cmp = journal::Comparer::with_run(1000);
    for at in [None, Some(0), Some(999), Some(1000), Some(4999)] {
        let mut other = data.clone();
        if let Some(at) = at {
            other[at] ^= 1;
        }
        fs::write(&b, &other).unwrap();
        let mut h = blake3_of();
        assert_eq!(cmp.same_with(&a, &b, true, Some(&mut h)).unwrap(), at.is_none(), "{at:?}");
        if at.is_none() {
            assert_eq!(h.finalize().to_hex().to_string(), journal::hash_file(&b).unwrap(), "the hash taken on the way is the file's");
        }
    }
}

fn blake3_of() -> media_core::blake3::Hasher {
    media_core::blake3::Hasher::new()
}

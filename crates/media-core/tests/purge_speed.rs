//! How long emptying a quarantine of many small files takes (`MO_PURGE_FILES`, default 20000).
//! `cargo test --release -p media-core --test purge_speed -- --ignored --nocapture`

use std::fs;
use std::time::Instant;

use media_core::scanner::{self, ScanOptions};
use media_core::{identity, journal, volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

#[test]
#[ignore]
fn many_small_files() {
    let n: usize = std::env::var("MO_PURGE_FILES").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000);
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    write_jpeg(&e.join("seed.jpg"), &picture(1, 400, 300), Some(("2019:05:01 10:00:00", "Canon", "EOS R6")));
    let seed = fs::read(e.join("seed.jpg")).unwrap();
    fs::remove_file(e.join("seed.jpg")).unwrap();
    for i in 0..n {
        let dir = format!("{:03}", i / 200);
        fs::create_dir_all(e.join("사진").join(&dir)).unwrap();
        fs::create_dir_all(e.join("백업").join(&dir)).unwrap();
        let mut b = seed.clone();
        b.extend_from_slice(format!("{i:016}").as_bytes());
        fs::write(e.join("사진").join(&dir).join(format!("p{i}.jpg")), &b).unwrap();
        fs::write(e.join("백업").join(&dir).join(format!("p{i}.jpg")), &b).unwrap();
    }
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    identity::backfill_prehashes(&conn).unwrap();
    let ids: Vec<i64> = conn.prepare("SELECT id FROM files WHERE rel_path LIKE '백업/%'").unwrap().query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
    assert_eq!(ids.len(), n);
    let rep = journal::delete_files(&db, &ids, false, None, &stop, &quiet).unwrap();
    assert_eq!(rep.deleted as usize, n);
    let t = Instant::now();
    let r = journal::purge_batch(&conn, &rep.batch_id).unwrap();
    println!("purge of {n} files ({} KB each): {:.2?}", seed.len() / 1024, t.elapsed());
    assert_eq!((r.restored as usize, r.failed), (n, 0), "{:?}", r.errors.first());
}

/// The ways of checking one large copy against its original (`MO_PURGE_MB`, default 1024).
#[test]
#[ignore]
fn one_large_file() {
    let mb: usize = std::env::var("MO_PURGE_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(1024);
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a.mp4"), tmp.path().join("b.mp4"));
    let block: Vec<u8> = (0..1usize << 20).map(|i| (i as u8).wrapping_mul(31).wrapping_add((i >> 8) as u8)).collect();
    {
        use std::io::Write;
        let mut f = fs::File::create(&a).unwrap();
        for _ in 0..mb {
            f.write_all(&block).unwrap();
        }
    }
    fs::copy(&a, &b).unwrap();
    let mut cmp = journal::Comparer::new();
    let t = Instant::now();
    assert!(cmp.same(&a, &b).unwrap());
    println!("{mb} MB, both read in turns: {:.2?}", t.elapsed());
    let t = Instant::now();
    assert!(cmp.same_with(&a, &b, true, None).unwrap());
    println!("{mb} MB, both read at the same time: {:.2?}", t.elapsed());
    let t = Instant::now();
    journal::hash_file(&a).unwrap();
    println!("{mb} MB, one read (the other's hash known): {:.2?}", t.elapsed());
    let t = Instant::now();
    assert!(cmp.same_sampled(&a, &b).unwrap());
    println!("{mb} MB, quick check: {:.2?}", t.elapsed());
}

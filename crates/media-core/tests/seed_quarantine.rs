//! Fills an app's library with a drive of photos and three batches waiting in the quarantine,
//! to look at 「격리함 전부 비우기」 in the app: `MO_SEED_DB=<library.db> MO_SEED_DIR=<folder>`.

use std::fs;

use media_core::scanner::{self, ScanOptions};
use media_core::{identity, journal, volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

#[test]
#[ignore]
fn seed() {
    let (Ok(dbp), Ok(dir)) = (std::env::var("MO_SEED_DB"), std::env::var("MO_SEED_DIR")) else { return };
    let e = drive(std::path::Path::new(&dir), "purge-demo");
    fs::create_dir_all(e.join("백업")).unwrap();
    for i in 0..30u32 {
        let p = e.join(format!("사진/p{i}.jpg"));
        write_jpeg(&p, &picture(900 + i, 2000, 1500), Some(("2021:03:01 10:00:00", "Canon", "EOS R6")));
        fs::copy(&p, e.join(format!("백업/p{i}.jpg"))).unwrap();
    }
    // One "copy" that is damaged after its beginning: the good one must survive.
    let mut bad = fs::read(e.join("사진/p0.jpg")).unwrap();
    let n = bad.len();
    for b in &mut bad[n / 2..] {
        *b = 0;
    }
    fs::write(e.join("백업/p0.jpg"), &bad).unwrap();
    let db = Db::open(&dbp).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    identity::backfill_prehashes(&conn).unwrap();
    for (lo, hi, dir) in [(0, 10, "사진"), (10, 20, "백업"), (20, 30, "백업")] {
        let ids: Vec<i64> = (lo..hi).map(|i| conn.query_row("SELECT id FROM files WHERE rel_path = ?", [format!("{dir}/p{i}.jpg")], |r| r.get(0)).unwrap()).collect();
        let rep = journal::delete_files(&db, &ids, false, None, &stop, &quiet).unwrap();
        println!("batch {}: {} files", rep.batch_id, rep.deleted);
    }
}

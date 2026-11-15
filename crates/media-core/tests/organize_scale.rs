//! Organizing a large library (run by hand, release build):
//! `cargo test --release -p media-core --test organize_scale -- --ignored --nocapture`

use std::fs;
use std::time::Instant;

use media_core::organizer::{self, Mode, OrganizeOptions};
use media_core::rusqlite::params;
use media_core::{volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

#[test]
#[ignore]
fn plan_and_move_many_files() {
    let n: usize = std::env::var("MO_SCALE_FILES").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000);
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let mut conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let vid: i64 = conn.query_row("SELECT volume_id FROM sources", [], |r| r.get(0)).unwrap();
    {
        let tx = conn.transaction().unwrap();
        for i in 0..n {
            let rel = format!("dump/{:03}/IMG_{i:06}.jpg", i / 100);
            let p = e.join(&rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, format!("photo {i}")).unwrap();
            // Spread over 5 years.
            let taken = 1_600_000_000 + (i as i64 % 1800) * 86_400;
            tx.execute(
                "INSERT INTO files(volume_id, rel_path, name, ext, kind, size, mtime, taken_at, date_source) VALUES (?1, ?2, ?3, 'jpg', 'photo', ?4, 0, ?5, 'exif')",
                params![vid, rel, format!("IMG_{i:06}.jpg"), fs::metadata(&p).unwrap().len() as i64, taken],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let opts = OrganizeOptions {
        target_dir: e.join("사진").to_string_lossy().into_owned(),
        mode: Mode::Move,
        ..Default::default()
    };
    let stop = StopFlag::new();
    let t = Instant::now();
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    let planned = t.elapsed();
    let t = Instant::now();
    let rep = organizer::execute(&db, &plan, &stop, &quiet).unwrap();
    let ran = t.elapsed();
    assert_eq!(rep.done as usize, n, "{:?}", rep.errors.first());
    let t = Instant::now();
    let again = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    let replanned = t.elapsed();
    assert_eq!(again.summary.in_place as usize, n);
    println!("{n} files: plan {planned:.2?}, move {ran:.2?}, plan again (all in place) {replanned:.2?}");
}

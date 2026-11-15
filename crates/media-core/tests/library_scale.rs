//! Library plan ("라이브러리로 모으기") on a large index (run by hand, release build):
//! `MO_SCALE_FILES=500000 cargo test --release -p media-core --test library_scale -- --ignored --nocapture`
//!
//! Index rows only (no files on disk): the plan must not need the disk, so this measures the
//! plan itself — copies, pairs, events, years.

use std::time::Instant;

use media_core::library::LibraryOptions;
use media_core::organizer::{self, OrganizeOptions};
use media_core::rusqlite::params;
use media_core::{volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

#[test]
#[ignore]
fn library_plan_many_files() {
    let n: usize = std::env::var("MO_SCALE_FILES").ok().and_then(|v| v.parse().ok()).unwrap_or(100_000);
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let c = drive(tmp.path(), "diskC");
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let mut conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    volumes::add_source(&conn, &c).unwrap();
    let vols: Vec<i64> = {
        let mut st = conn.prepare("SELECT volume_id FROM sources ORDER BY id").unwrap();
        let v = st.query_map([], |r| r.get(0)).unwrap().collect::<Result<Vec<i64>, _>>().unwrap();
        v
    };
    let (ve, vc) = (vols[0], vols[1]);
    let t = Instant::now();
    {
        let tx = conn.transaction().unwrap();
        {
            let mut st = tx
                .prepare(
                    "INSERT INTO files(volume_id, rel_path, name, ext, kind, size, mtime, taken_at, date_source, prehash)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, 'exif', ?8)",
                )
                .unwrap();
            let mut originals = 0usize;
            let mut i = 0usize;
            while i < n {
                // An original: in an event folder (half) or a camera dump, over 15 years.
                let day = (originals % 5400) as i64;
                let taken = 1_100_000_000 + day * 86_400 + (originals as i64 % 3600);
                let year = 2004 + day / 360;
                let dir = if originals.is_multiple_of(2) {
                    format!("옛날/{year}-{:02} 여행 {}", (day / 30) % 12 + 1, originals / 200)
                } else {
                    format!("DCIM/{:03}CANON", 100 + (originals / 500) % 900)
                };
                let name = format!("IMG_{originals:07}");
                let size = 2_000_000 + (originals as i64 * 7919) % 3_000_000;
                let key = format!("ph{originals}");
                st.execute(params![ve, format!("{dir}/{name}.JPG"), format!("{name}.JPG"), "jpg", "photo", size, taken, key]).unwrap();
                i += 1;
                // Every 10th has a RAW beside it.
                if originals.is_multiple_of(10) && i < n {
                    st.execute(params![ve, format!("{dir}/{name}.CR2"), format!("{name}.CR2"), "cr2", "raw", size * 3, taken, format!("raw{originals}")])
                        .unwrap();
                    i += 1;
                }
                // ~30 % copies on E (a backup folder), ~10 % on C.
                if originals % 10 < 3 && i < n {
                    st.execute(params![ve, format!("백업/폰사진 {}/{name}.JPG", originals / 1000), format!("{name}.JPG"), "jpg", "photo", size, taken, key])
                        .unwrap();
                    i += 1;
                }
                if originals % 10 == 5 && i < n {
                    st.execute(params![vc, format!("Users/me/Pictures/{name}.JPG"), format!("{name}.JPG"), "jpg", "photo", size, taken, key]).unwrap();
                    i += 1;
                }
                originals += 1;
            }
        }
        tx.commit().unwrap();
    }
    println!("{n} index rows written in {:.2?}", t.elapsed());
    let opts = OrganizeOptions {
        target_dir: e.join("사진").to_string_lossy().into_owned(),
        layout: "library".into(),
        library: LibraryOptions::default(),
        ..Default::default()
    };
    let stop = StopFlag::new();
    let t = Instant::now();
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    let planned = t.elapsed();
    let s = &plan.summary;
    println!(
        "{n} files: plan {planned:.2?} — moves {} copies {} remove {} leave {} events {} · timing {:?}",
        s.moves, s.copies, s.remove, s.leave, s.events, s.timing
    );
    let t = Instant::now();
    let years = organizer::year_buckets(&plan);
    let y = years.iter().find(|y| y.key == "2010").unwrap();
    let _ = organizer::year_copies(&plan, &y.key);
    let _ = organizer::subset_year(&plan, &y.key);
    println!("years {} · year views {:.2?}", years.len(), t.elapsed());
    assert!(s.remove > 0 && s.moves > 0);
}

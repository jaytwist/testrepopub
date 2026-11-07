//! How long people grouping takes on a large library (run by hand, release build):
//! `cargo test --release -p media-core --test people_scale -- --ignored --nocapture`
//! `MO_SCALE_FACES` sets the face count (default 244,490 — a real user's library).

use std::time::Instant;

use media_core::analysis::embedding_to_blob;
use media_core::{people, Db};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f32 {
        let (u, v) = (self.next().max(1e-12), self.next());
        ((-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()) as f32
    }
    fn unit(&mut self) -> Vec<f32> {
        let v: Vec<f32> = (0..128).map(|_| self.gauss()).collect();
        media_core::faces::l2_normalize(&v)
    }
}

#[test]
#[ignore]
fn group_a_large_library() {
    let total: usize = std::env::var("MO_SCALE_FACES").ok().and_then(|v| v.parse().ok()).unwrap_or(244_490);
    // Half the faces belong to 1,500 recurring people, half are one-off strangers (the worst case
    // for grouping: each becomes its own small group).
    let people = 1_500;
    let known = total / 2;
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(tmp.path().join("library.db")).unwrap();
    let mut conn = db.conn().unwrap();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let centers: Vec<Vec<f32>> = (0..people).map(|_| rng.unit()).collect();
    {
        let tx = conn.transaction().unwrap();
        tx.execute("INSERT INTO volumes(uid, label, mount, online) VALUES ('v', 'v', '/', 1)", []).unwrap();
        let mut file = tx
            .prepare("INSERT INTO files(volume_id, rel_path, name, ext, kind, size, mtime, taken_at, date_source, width, height)
                      VALUES (1, ?1, ?1, 'jpg', 'photo', 1, 0, ?2, 'exif', 4000, 3000)")
            .unwrap();
        let mut face = tx
            .prepare("INSERT INTO faces(file_id, x, y, w, h, score, model, embedding) VALUES (?1, 0.4, 0.4, 0.1, 0.1, 0.95, 'sface-2021dec', ?2)")
            .unwrap();
        for i in 0..total {
            file.execute(rusqlite::params![format!("f{}/{i}.jpg", i / 200), 1_600_000_000 + (i / 200) as i64 * 86_400]).unwrap();
            let id = tx.last_insert_rowid();
            let v = if i < known {
                let c = &centers[i % people];
                let n: Vec<f32> = c.iter().map(|x| x + 0.10 * rng.gauss()).collect();
                media_core::faces::l2_normalize(&n)
            } else {
                rng.unit()
            };
            face.execute(rusqlite::params![id, embedding_to_blob(&v)]).unwrap();
        }
        drop(file);
        drop(face);
        tx.commit().unwrap();
    }
    let t = Instant::now();
    let s = people::refresh(&mut conn).unwrap();
    let first = t.elapsed();
    let t = Instant::now();
    let s2 = people::refresh(&mut conn).unwrap();
    let second = t.elapsed();
    // Settle (joins change references), then a run with nothing new skips the grouping.
    while people::refresh(&mut conn).unwrap().assigned > 0 {}
    let t = Instant::now();
    let s3 = people::refresh(&mut conn).unwrap();
    let idle = t.elapsed();
    assert_eq!((s3.assigned, s3.new_people), (0, 0));
    println!("nothing new: {idle:.1?}");
    let persons: i64 = conn.query_row("SELECT COUNT(*) FROM persons", [], |r| r.get(0)).unwrap();
    println!("faces {total}: first {first:.1?} ({s:?}), again {second:.1?} ({s2:?}), persons {persons}");
}

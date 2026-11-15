//! Similar photos from stored fingerprints: the fast search finds exactly the groups a
//! compare-everything search finds, and is fast on a large library.

use std::collections::{BTreeSet, HashMap};
use std::time::Instant;

use media_core::rusqlite::{params, Connection};
use media_core::{compare, Db};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// `n` photos: some families of near-identical hashes (versions), the rest random.
fn library(n: usize, seed: u64) -> (tempfile::TempDir, Db, Vec<(i64, u64)>) {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(tmp.path().join("library.db")).unwrap();
    let mut conn = db.conn().unwrap();
    let mut rng = Rng(seed);
    let mut rows = Vec::new();
    let tx = conn.transaction().unwrap();
    tx.execute("INSERT INTO volumes(uid, label, mount, online) VALUES ('v', 'v', '/', 1)", []).unwrap();
    let mut base = rng.next();
    for i in 0..n {
        // Every 5th photo starts a new family; others are 0–4 bits away from its base.
        let h = if i % 5 == 0 {
            base = rng.next();
            base
        } else if i % 7 == 0 {
            rng.next()
        } else {
            let mut h = base;
            for _ in 0..(rng.next() % 5) {
                h ^= 1 << (rng.next() % 64);
            }
            h
        };
        tx.execute(
            "INSERT INTO files(volume_id, rel_path, name, ext, kind, size, mtime, width, height, prehash) VALUES (1, ?1, ?1, 'jpg', 'photo', ?2, 0, 4000, 3000, ?3)",
            params![format!("{i}.jpg"), 1000 + i as i64, format!("p{i}")],
        )
        .unwrap();
        let id = tx.last_insert_rowid();
        tx.execute("INSERT INTO analysis(file_id, version, dhash, analyzed_at) VALUES (?, 1, ?, 0)", params![id, h as i64]).unwrap();
        rows.push((id, h));
    }
    tx.commit().unwrap();
    (tmp, db, rows)
}

/// Connected components by comparing every pair.
fn brute(rows: &[(i64, u64)], max_dist: u32) -> BTreeSet<Vec<i64>> {
    let mut parent: Vec<usize> = (0..rows.len()).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            x = p[x];
        }
        x
    }
    for a in 0..rows.len() {
        for b in a + 1..rows.len() {
            if (rows[a].1 ^ rows[b].1).count_ones() <= max_dist {
                let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
                if ra != rb {
                    parent[ra.max(rb)] = ra.min(rb);
                }
            }
        }
    }
    let mut g: HashMap<usize, Vec<i64>> = HashMap::new();
    for i in 0..rows.len() {
        let r = find(&mut parent, i);
        g.entry(r).or_default().push(rows[i].0);
    }
    g.into_values().filter(|v| v.len() > 1).map(|mut v| {
        v.sort();
        v
    }).collect()
}

fn fast(conn: &Connection, max_dist: u32) -> BTreeSet<Vec<i64>> {
    compare::similar_groups(conn, max_dist).unwrap().into_iter().map(|mut v| {
        v.sort();
        v
    }).collect()
}

#[test]
fn same_groups_as_comparing_everything() {
    for (seed, dist) in [(1u64, 1u32), (2, 3), (3, 5)] {
        let (_t, db, rows) = library(1500, seed);
        let conn = db.conn().unwrap();
        assert_eq!(fast(&conn, dist), brute(&rows, dist), "seed {seed} dist {dist}");
    }
}

#[test]
#[ignore]
fn fast_on_a_large_library() {
    let (_t, db, _rows) = library(300_000, 7);
    let conn = db.conn().unwrap();
    let t = Instant::now();
    let groups = compare::similar_groups(&conn, 3).unwrap();
    println!("300,000 photos: {} groups in {:.2?}", groups.len(), t.elapsed());
}

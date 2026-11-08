//! Cleanup on a large library (run by hand, release build):
//! `cargo test --release -p media-core --test cleanup_scale -- --ignored --nocapture`
//! 300,000 files, 5,000 groups of two identical copies on one drive.

use std::time::Instant;

use media_core::rusqlite::params;
use media_core::{dedupe, protect, Db};

#[test]
#[ignore]
fn protection_and_rules_at_scale() {
    let (files, groups) = (300_000i64, 5_000i64);
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(tmp.path().join("library.db")).unwrap();
    let mut conn = db.conn().unwrap();
    let mut copies: Vec<i64> = Vec::new();
    {
        let tx = conn.transaction().unwrap();
        tx.execute("INSERT INTO volumes(uid, label, mount, online) VALUES ('v', 'v', '/', 1)", []).unwrap();
        let mut ins = tx
            .prepare("INSERT INTO files(volume_id, rel_path, name, ext, kind, size, mtime, prehash) VALUES (1, ?1, ?1, 'jpg', 'photo', ?2, 0, ?3)")
            .unwrap();
        for i in 0..files {
            // The first 2 × `groups` files come in identical pairs.
            let key = if i < 2 * groups { format!("dup{}", i / 2) } else { format!("u{i}") };
            ins.execute(params![format!("p/{i}.jpg"), 1000 + i, key]).unwrap();
            if i < 2 * groups {
                copies.push(tx.last_insert_rowid());
            }
        }
        drop(ins);
        let mut g = tx.prepare("INSERT INTO dup_groups(kind, created_at) VALUES ('exact', 0)").unwrap();
        let mut m = tx.prepare("INSERT INTO dup_members(group_id, file_id, keep, rank) VALUES (?, ?, ?, ?)").unwrap();
        for pair in copies.chunks(2) {
            g.execute([]).unwrap();
            let gid = tx.last_insert_rowid();
            m.execute(params![gid, pair[0], 1, 0]).unwrap();
            m.execute(params![gid, pair[1], 0, 1]).unwrap();
        }
        drop((g, m));
        tx.commit().unwrap();
    }

    // Deleting every copy at once: one of each pair must be protected as the last copy.
    let t = Instant::now();
    let blocked = protect::check(&conn, &copies).unwrap();
    let check = t.elapsed();
    assert_eq!(blocked.len(), groups as usize);

    // Re-deciding every group (the one-time upgrade at startup) must actually redo the keepers.
    conn.execute("UPDATE dup_members SET keep = 0", []).unwrap();
    let t = Instant::now();
    let changed = dedupe::apply_rules(&mut conn).unwrap();
    let rules = t.elapsed();
    assert_eq!(changed, groups as usize);
    let kept: i64 = conn.query_row("SELECT COUNT(*) FROM dup_members WHERE keep = 1", [], |r| r.get(0)).unwrap();
    assert_eq!(kept, groups);
    println!("files {files}, copies {}: protect::check {check:.2?}, apply_rules {rules:.2?}", copies.len());
}

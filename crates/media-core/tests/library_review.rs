//! The year's delete list: identical copies (복사본 = same name, 사본 = another name), similar
//! photos (resized / recompressed — not deleted on their own, reviewed per year), and photos the
//! user put on the list by hand, deleted for good on Push.

use std::fs;
use std::path::Path;

use media_core::dedupe::{self, Similarity};
use media_core::library::LibraryOptions;
use media_core::organizer::{self, Action, ExecOptions, OrganizeOptions};
use media_core::rusqlite::params;
use media_core::scanner::{self, ScanOptions};
use media_core::triage::{self, Decision};
use media_core::{volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

fn jpeg(p: &Path, seed: u32, w: u32, h: u32, date: &str) {
    write_jpeg(p, &picture(seed, w, h), Some((date, "Canon", "EOS R6")));
}

#[test]
fn year_delete_list() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let c = drive(tmp.path(), "diskC");
    // 2015: an original, a resized version of it (messenger), a copy with the same name and one
    // renamed, and a photo the user will pick by hand (also on C).
    jpeg(&e.join("옛날/2015-08 제주도/a.jpg"), 1, 1600, 1200, "2015:08:10 10:00:00");
    jpeg(&e.join("옛날/2015-08 제주도/a_small.jpg"), 1, 800, 600, "2015:08:10 10:00:00");
    jpeg(&e.join("옛날/2015-08 제주도/b.jpg"), 2, 1600, 1200, "2015:08:11 10:00:00");
    fs::create_dir_all(e.join("백업")).unwrap();
    fs::copy(e.join("옛날/2015-08 제주도/a.jpg"), e.join("백업/a.jpg")).unwrap();
    fs::copy(e.join("옛날/2015-08 제주도/a.jpg"), e.join("백업/a (1).jpg")).unwrap();
    fs::create_dir_all(c.join("Pictures")).unwrap();
    fs::copy(e.join("옛날/2015-08 제주도/b.jpg"), c.join("Pictures/b.jpg")).unwrap();
    // 2016: a similar pair of its own.
    // A 2015 photo only on C, put on the list by hand: C is never touched — it is just not brought in.
    jpeg(&c.join("Pictures/c_only.jpg"), 4, 1600, 1200, "2015:08:12 10:00:00");
    jpeg(&e.join("옛날/2016-05 부산/p.jpg"), 3, 1600, 1200, "2016:05:01 10:00:00");
    jpeg(&e.join("옛날/2016-05 부산/p_small.jpg"), 3, 800, 600, "2016:05:01 10:00:00");

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    volumes::add_source(&conn, &c).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    let id = |name: &str| -> i64 { conn.query_row("SELECT id FROM files WHERE rel_path LIKE ?", [format!("%{name}")], |r| r.get(0)).unwrap() };
    // Fingerprints of the look (as analysis stores them): each original and its smaller version alike.
    let (ha, hp) = (0i64, i64::MAX); // 63 bits apart: never alike
    for (n, h) in [("제주도/a.jpg", ha), ("a_small.jpg", ha), ("백업/a.jpg", ha), ("a (1).jpg", ha), ("부산/p.jpg", hp), ("p_small.jpg", hp)] {
        conn.execute("INSERT OR REPLACE INTO analysis(file_id, version, dhash, analyzed_at) VALUES (?, 1, ?, 0)", params![id(n), h]).unwrap();
    }
    dedupe::find_similar_images(&db, Similarity::High, &stop, &quiet).unwrap();

    let opts = OrganizeOptions {
        target_dir: e.join("사진").to_string_lossy().into_owned(),
        layout: "library".into(),
        library: LibraryOptions::default(),
        ..Default::default()
    };
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    let item = |plan: &organizer::Plan, rel: &str| plan.items.iter().find(|i| i.src.replace('\\', "/").ends_with(rel)).cloned().unwrap();

    // The smaller version is not deleted on its own: it goes beside its original.
    let small = item(&plan, "a_small.jpg");
    assert_eq!((small.action, small.tag.as_str()), (Action::Move, "version"), "{small:?}");
    assert_eq!(small.group, item(&plan, "제주도/a.jpg").group);

    // 복사본 (same name) and 사본 (renamed), both identical copies.
    let folders = organizer::year_copies(&plan, "2015");
    let backup = folders.iter().find(|f| f.dir.replace('\\', "/").ends_with("/백업")).unwrap();
    assert_eq!((backup.count, backup.same_name), (2, 1), "{backup:?}");

    // Review groups of 2015 only (2016's pair is not offered).
    let files15 = organizer::year_files(&plan, "2015");
    let lib = organizer::lib_vol(&plan).unwrap();
    let groups = triage::groups_in(&conn, "similar_image", &files15, lib, 0, 50).unwrap();
    assert_eq!(groups.len(), 1, "{groups:?}");
    assert!(groups[0].members.iter().any(|m| m.file.id == id("a_small.jpg")));
    assert_eq!(triage::count_in(&conn, "similar_image", &organizer::year_files(&plan, "2016"), lib).unwrap(), 1);

    // A new search keeps what was decided: keep only the big one in 2016's group.
    let g16 = triage::groups_in(&conn, "similar_image", &organizer::year_files(&plan, "2016"), lib, 0, 50).unwrap();
    triage::commit(&mut db.conn().unwrap(), &[Decision { key: g16[0].key.clone(), keep: vec![id("부산/p.jpg")] }]).unwrap();
    dedupe::find_similar_images(&db, Similarity::High, &stop, &quiet).unwrap();
    let kept: Vec<(i64, i64)> = {
        let mut st = conn
            .prepare("SELECT m.file_id, m.keep FROM dup_members m JOIN dup_groups g ON g.id = m.group_id WHERE g.kind = 'similar_image' AND m.file_id IN (?, ?)")
            .unwrap();
        let v = st.query_map(params![id("부산/p.jpg"), id("p_small.jpg")], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<Result<_, _>>().unwrap();
        v
    };
    assert!(kept.contains(&(id("p_small.jpg"), 0)), "the decision survives a new search: {kept:?}");

    // ♥ on b; push 2015 with the smaller version (reviewed) and b (picked by hand) on the list.
    conn.execute("INSERT INTO labels(file_id, label, source) VALUES (?, 'favorite', 'user')", [id("제주도/b.jpg")]).unwrap();
    let (small_id, b_id, c_id) = (id("a_small.jpg"), id("제주도/b.jpg"), id("c_only.jpg"));
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    let xo = ExecOptions {
        purge: true,
        label: Some("2015년".into()),
        drop_ids: vec![small_id, b_id, c_id],
        decisions: vec![Decision { key: groups[0].key.clone(), keep: vec![id("제주도/a.jpg")] }],
    };
    let rep = organizer::execute_with(&db, &organizer::subset_year(&plan, "2015"), &xo, &stop, &quiet).unwrap();
    assert_eq!(rep.failed, 0, "the deleted photo is not moved afterwards: {:?}", rep.errors);
    assert_eq!((rep.dropped, rep.drop_kept), (1, 1), "the smaller version goes, ♥ b stays: {rep:?}");
    assert_eq!(rep.purged, 2, "both copies in 백업 deleted");
    assert!(!e.join("옛날/2015-08 제주도/a_small.jpg").exists());
    assert!(!e.join("사진/2015/2015-08/제주도/a_small.jpg").exists());
    assert!(e.join("사진/2015/2015-08/제주도/a.jpg").is_file());
    assert!(e.join("사진/2015/2015-08/제주도/b.jpg").is_file(), "♥ kept and organized");
    assert!(c.join("Pictures/b.jpg").is_file());
    assert_eq!(rep.not_brought, 1);
    assert!(c.join("Pictures/c_only.jpg").is_file(), "C is never touched");
    assert!(!e.join("사진/2015/2015-08/c_only.jpg").exists(), "and not brought in");

    // Planning again: 2015 is done; the reviewed group is not offered again.
    let again = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    let y = organizer::year_buckets(&again).into_iter().find(|y| y.key == "2015").unwrap();
    assert!(y.done, "{y:?}");
    let cc = again.items.iter().find(|i| i.src.ends_with("c_only.jpg")).unwrap();
    assert_eq!((cc.action, cc.tag.as_str()), (Action::Leave, "excluded"), "not brought in later either");
    let files15 = organizer::year_files(&again, "2015");
    assert_eq!(triage::groups_in(&conn, "similar_image", &files15, lib, 0, 50).unwrap().len(), 0);
}

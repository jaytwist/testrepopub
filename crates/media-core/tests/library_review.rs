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
    triage::commit(&mut db.conn().unwrap(), &[Decision { key: g16[0].key.clone(), keep: vec![id("부산/p.jpg")], ..Default::default() }]).unwrap();
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
        decisions: vec![Decision { key: groups[0].key.clone(), keep: vec![id("제주도/a.jpg")], drop: vec![small_id] }],
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

/// Similar-photo groups are found again after every analysis pass. A review in progress refers to
/// groups by id, so: an unchanged group keeps its id, a deleted group's id never comes back as
/// another group, and a decision about a group that changed is not saved onto anything.
#[test]
fn group_ids_are_stable_and_stale_decisions_do_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    for (n, seed) in [("a", 1), ("a2", 1), ("b", 2), ("b2", 2), ("c", 3), ("c2", 3)] {
        let (w, h) = if n.ends_with('2') { (800, 600) } else { (1600, 1200) };
        jpeg(&e.join(format!("p/{n}.jpg")), seed, w, h, "2015:08:10 10:00:00");
    }
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    let id = |name: &str| -> i64 { conn.query_row("SELECT id FROM files WHERE name = ?", [name], |r| r.get(0)).unwrap() };
    let look = |name: &str, h: i64| {
        conn.execute("INSERT OR REPLACE INTO analysis(file_id, version, dhash, analyzed_at) VALUES (?, 1, ?, 0)", params![id(name), h]).unwrap();
    };
    let group_of = |name: &str| -> Option<i64> {
        conn.query_row(
            "SELECT m.group_id FROM dup_members m JOIN dup_groups g ON g.id = m.group_id WHERE g.kind = 'similar_image' AND g.resolved = 0 AND m.file_id = ?",
            [id(name)],
            |r| r.get(0),
        )
        .ok()
    };
    let keeps = |gid: i64| -> Vec<(i64, i64)> {
        let mut st = conn.prepare("SELECT file_id, keep FROM dup_members WHERE group_id = ? ORDER BY file_id").unwrap();
        let v = st.query_map([gid], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<Result<_, _>>().unwrap();
        v
    };
    // Two groups: a + a2, b + b2 (c and c2 not analysed yet).
    look("a.jpg", 0);
    look("a2.jpg", 0);
    look("b.jpg", 0x0000_FFFF_0000_FFFF);
    look("b2.jpg", 0x0000_FFFF_0000_FFFF);
    dedupe::find_similar_images(&db, Similarity::High, &stop, &quiet).unwrap();
    let (ga, gb) = (group_of("a.jpg").unwrap(), group_of("b.jpg").unwrap());

    // Found again with nothing changed: the same ids.
    dedupe::find_similar_images(&db, Similarity::High, &stop, &quiet).unwrap();
    assert_eq!((group_of("a.jpg"), group_of("b.jpg")), (Some(ga), Some(gb)));

    // The user opens a review of group b (the newest id)…
    let stale = Decision { key: format!("dup:{gb}"), keep: vec![id("b.jpg")], drop: vec![id("b2.jpg")] };
    // …meanwhile b2 is gone (so b's group dissolves) and c + c2 get analysed: a new group.
    fs::remove_file(e.join("p/b2.jpg")).unwrap();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    look("c.jpg", 0x00FF_00FF_00FF_00FF);
    look("c2.jpg", 0x00FF_00FF_00FF_00FF);
    dedupe::find_similar_images(&db, Similarity::High, &stop, &quiet).unwrap();
    assert_eq!(group_of("a.jpg"), Some(ga), "an unchanged group keeps its id");
    assert_eq!(group_of("b.jpg"), None);
    let gc = group_of("c.jpg").unwrap();
    assert!(gc > gb, "a deleted group's id is not used again ({gc} vs {gb})");

    // The stale decision is saved onto nothing.
    let before = keeps(gc);
    triage::commit(&mut db.conn().unwrap(), std::slice::from_ref(&stale)).unwrap();
    assert_eq!(keeps(gc), before);
    // Even addressed at the new group's id, a decision that keeps none of its photos is refused.
    triage::commit(&mut db.conn().unwrap(), &[Decision { key: format!("dup:{gc}"), keep: vec![id("b.jpg")], ..Default::default() }]).unwrap();
    assert_eq!(keeps(gc), before);
    triage::commit(&mut db.conn().unwrap(), &[Decision { key: format!("dup:{gc}"), keep: vec![], ..Default::default() }]).unwrap();
    assert_eq!(keeps(gc), before);

    // Push with a decision whose kept photo no longer exists: its drops are spared.
    let opts = OrganizeOptions {
        target_dir: e.join("사진").to_string_lossy().into_owned(),
        layout: "library".into(),
        library: LibraryOptions::default(),
        ..Default::default()
    };
    fs::remove_file(e.join("p/c.jpg")).unwrap();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    let xo = ExecOptions {
        drop_ids: vec![id("c2.jpg")],
        decisions: vec![Decision { key: format!("dup:{gc}"), keep: vec![id("c.jpg")], drop: vec![id("c2.jpg")] }],
        ..Default::default()
    };
    let rep = organizer::execute_with(&db, &organizer::subset_year(&plan, "2015"), &xo, &stop, &quiet).unwrap();
    assert_eq!(rep.dropped, 0, "{rep:?}");
    let left: Vec<String> = walkdir(&e).into_iter().filter(|p| p.ends_with("c2.jpg")).collect();
    assert_eq!(left.len(), 1, "the last of its group is not deleted: {left:?} {:?}", rep.errors);
    assert!(!left[0].contains("quarantine"), "{left:?}");
}

fn walkdir(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out
}

/// For a copy: which file is the original, where it is, what each of the others is, and why —
/// and it is the same file the organizer keeps.
#[test]
fn a_copy_knows_its_original() {
    use media_core::identity;
    use media_core::importer::ImportSettings;
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let c = drive(tmp.path(), "diskC");
    jpeg(&e.join("옛날/2015-08 제주도/a.jpg"), 1, 1600, 1200, "2015:08:10 10:00:00");
    fs::create_dir_all(e.join("백업")).unwrap();
    fs::copy(e.join("옛날/2015-08 제주도/a.jpg"), e.join("백업/a.jpg")).unwrap();
    fs::copy(e.join("옛날/2015-08 제주도/a.jpg"), e.join("백업/a (1).jpg")).unwrap();
    fs::create_dir_all(c.join("Pictures")).unwrap();
    fs::copy(e.join("옛날/2015-08 제주도/a.jpg"), c.join("Pictures/a.jpg")).unwrap();
    jpeg(&e.join("옛날/2015-08 제주도/alone.jpg"), 2, 1600, 1200, "2015:08:11 10:00:00");

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    volumes::add_source(&conn, &c).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    let lib = e.join("사진");
    ImportSettings { target_dir: lib.to_string_lossy().into_owned(), ..Default::default() }.save(&conn).unwrap();
    identity::refresh_roles(&mut db.conn().unwrap()).unwrap();
    let id = |like: &str| -> i64 { conn.query_row("SELECT id FROM files WHERE rel_path LIKE ?", [format!("%{like}")], |r| r.get(0)).unwrap() };

    let fam = identity::family(&conn, id("백업/a (1).jpg")).unwrap().unwrap();
    assert_eq!(fam.kind, "exact");
    let said: Vec<(String, String)> = fam.members.iter().map(|m| (m.file.rel_path.clone(), m.relation.clone())).collect();
    assert_eq!(fam.members[0].file.id, id("제주도/a.jpg"), "the one in the named folder is the original: {said:?}");
    assert_eq!(fam.why, "라이브러리 드라이브에 있음 · 이름 있는 폴더(이벤트)에 있음");
    let rel = |like: &str| fam.members.iter().find(|m| m.abs_path.replace('\\', "/").ends_with(like)).map(|m| m.relation.as_str()).unwrap();
    assert_eq!((rel("백업/a.jpg"), rel("백업/a (1).jpg"), rel("Pictures/a.jpg")), ("복사본", "사본", "백업"));
    assert!(fam.members.iter().find(|m| m.is_self).is_some_and(|m| m.relation == "사본"));
    assert!(identity::family(&conn, id("alone.jpg")).unwrap().is_none(), "a photo with no copies has no family");

    // The organizer keeps that same file; once filed, that is the reason shown.
    let opts = OrganizeOptions { target_dir: lib.to_string_lossy().into_owned(), layout: "library".into(), library: LibraryOptions::default(), ..Default::default() };
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    let kept: Vec<i64> = plan.items.iter().filter(|i| i.action == Action::Remove).filter_map(|i| i.kept).collect();
    assert!(!kept.is_empty() && kept.iter().all(|k| *k == fam.members[0].file.id), "{kept:?}");
    organizer::execute(&db, &plan, &stop, &quiet).unwrap();
    identity::refresh_roles(&mut db.conn().unwrap()).unwrap();
    let fam = identity::family(&conn, id("Pictures/a.jpg")).unwrap().unwrap();
    assert!(fam.members[0].abs_path.replace('\\', "/").ends_with("사진/2015/2015-08/제주도/a.jpg"), "{:?}", fam.members[0].abs_path);
    assert_eq!(fam.why, "라이브러리에 정리되어 있음");
}

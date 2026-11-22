//! 「한 곳으로 정리」 inside a work scope: only the scope's photos are planned, into the main
//! library; one already filed there stands for its copies (nothing is brought in twice, and an
//! edited version goes beside it); of several edits the newest stays, the others are offered
//! for deletion in the year's review.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use media_core::db::Scope;
use media_core::dedupe::{self, Similarity};
use media_core::library::LibraryOptions;
use media_core::organizer::{self, Action, OrganizeOptions};
use media_core::rusqlite::params;
use media_core::scanner::{self, ScanOptions};
use media_core::triage::{self, YearReview};
use media_core::{volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

fn jpeg(p: &Path, seed: u32, date: &str) {
    write_jpeg(p, &picture(seed, 1600, 1200), Some((date, "Canon", "EOS R6")));
}

fn touch(p: &Path, secs_ago: u64) {
    let f = fs::File::options().write(true).open(p).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(secs_ago)).unwrap();
}

#[test]
fn a_scope_goes_into_the_library_once() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    // Already filed in the library.
    jpeg(&e.join("사진/2019/2019-05/제주/a.jpg"), 1, "2019:05:10 10:00:00");
    // The recovery folder: a copy of a (another name), a new photo, and three edits of it.
    fs::create_dir_all(e.join("RAID_Recovery/x")).unwrap();
    fs::copy(e.join("사진/2019/2019-05/제주/a.jpg"), e.join("RAID_Recovery/x/f0001.jpg")).unwrap();
    jpeg(&e.join("RAID_Recovery/x/2018-07 부산/b.jpg"), 2, "2018:07:01 10:00:00");
    for (i, n) in ["b_edit1.jpg", "b_edit2.jpg", "b_edit3.jpg"].iter().enumerate() {
        jpeg(&e.join("RAID_Recovery/x/2018-07 부산").join(n), 10 + i as u32, "2018:07:01 10:00:00");
    }
    touch(&e.join("RAID_Recovery/x/2018-07 부산/b_edit1.jpg"), 3000);
    touch(&e.join("RAID_Recovery/x/2018-07 부산/b_edit2.jpg"), 100); // the newest edit
    touch(&e.join("RAID_Recovery/x/2018-07 부산/b_edit3.jpg"), 2000);
    // An edit of the filed photo, in the recovery folder.
    jpeg(&e.join("RAID_Recovery/x/a_edit.jpg"), 20, "2019:05:10 10:00:00");
    // Outside the scope.
    jpeg(&e.join("옛날/2010-01 서울/c.jpg"), 3, "2010:01:05 10:00:00");

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    media_core::identity::backfill_prehashes(&conn).unwrap();
    let id = |rel: &str| -> i64 { conn.query_row("SELECT id FROM files WHERE rel_path = ?", [rel], |r| r.get(0)).unwrap() };
    // b and its edits look alike, and so do a and its edit (as analysis would find).
    let (hb, ha) = (0i64, i64::MAX);
    for (r, h) in [
        ("RAID_Recovery/x/2018-07 부산/b.jpg", hb),
        ("RAID_Recovery/x/2018-07 부산/b_edit1.jpg", hb),
        ("RAID_Recovery/x/2018-07 부산/b_edit2.jpg", hb),
        ("RAID_Recovery/x/2018-07 부산/b_edit3.jpg", hb),
        ("사진/2019/2019-05/제주/a.jpg", ha),
        ("RAID_Recovery/x/a_edit.jpg", ha),
    ] {
        conn.execute("INSERT OR REPLACE INTO analysis(file_id, version, dhash, analyzed_at) VALUES (?, 1, ?, 0)", params![id(r), h]).unwrap();
    }
    dedupe::find_similar_images(&db, Similarity::High, &stop, &quiet).unwrap();
    // Mark the edits as edits (the look of the test pictures says nothing about it).
    for r in ["b_edit1.jpg", "b_edit2.jpg", "b_edit3.jpg", "a_edit.jpg"] {
        conn.execute("UPDATE dup_members SET variant = 'edited' WHERE file_id = (SELECT id FROM files WHERE name = ?)", [r]).unwrap();
    }
    // … and the originals as the originals.
    for r in ["b.jpg", "a.jpg"] {
        conn.execute("UPDATE dup_members SET variant = NULL, rank = -1, keep = 1 WHERE file_id = (SELECT id FROM files WHERE name = ?)", [r]).unwrap();
    }
    let vol: i64 = conn.query_row("SELECT id FROM volumes", [], |r| r.get(0)).unwrap();
    let scope = Scope::new(vol, "RAID_Recovery");

    let lib = e.join("사진");
    let mut opts = OrganizeOptions { target_dir: lib.to_string_lossy().into_owned(), layout: "library".into(), library: LibraryOptions::default(), ..Default::default() };
    // Without a scope first: the result the scoped plan must agree with.
    let whole = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    opts.filter.volume_id = Some(scope.volume_id);
    opts.filter.under = Some(scope.dir.clone());
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    let item = |p: &organizer::Plan, end: &str| p.items.iter().find(|i| i.src.replace('\\', "/").ends_with(end)).cloned();
    let rel = |p: &str| Path::new(p).strip_prefix(&lib).map(|x| x.to_string_lossy().replace('\\', "/")).unwrap_or_default();

    // (a) Only the scope.
    assert!(plan.items.iter().all(|i| i.src.replace('\\', "/").contains("/RAID_Recovery/")), "{:#?}", plan.items.iter().map(|i| &i.src).collect::<Vec<_>>());
    assert!(item(&plan, "옛날/2010-01 서울/c.jpg").is_none() && item(&plan, "제주/a.jpg").is_none());
    // (b) The copy of the filed photo goes; nothing is brought in twice.
    let copy = item(&plan, "x/f0001.jpg").unwrap();
    assert_eq!(copy.action, Action::Remove, "{copy:?}");
    // (c) The new photo goes to year / month / topic; edits beside it; the filed photo's edit beside it.
    let b = item(&plan, "부산/b.jpg").unwrap();
    assert_eq!((b.action, rel(&b.dst).as_str()), (Action::Move, "2018/2018-07/부산/b.jpg"), "{b:?}");
    let e2 = item(&plan, "b_edit2.jpg").unwrap();
    assert_eq!(rel(&e2.dst), "2018/2018-07/부산/b_edit2.jpg");
    let ae = item(&plan, "a_edit.jpg").unwrap();
    assert_eq!((ae.action, rel(&ae.dst).as_str()), (Action::Move, "2019/2019-05/제주/a_edit.jpg"), "{ae:?}");
    // (g) The scoped plan places its photos as the whole plan does.
    for end in ["부산/b.jpg", "b_edit2.jpg", "x/f0001.jpg", "a_edit.jpg"] {
        let (w, s) = (item(&whole, end).unwrap(), item(&plan, end).unwrap());
        assert_eq!((w.action, w.dst), (s.action, s.dst), "{end}");
    }

    // (d) The 2018 review: the original and the newest edit are kept, the two older edits offered.
    let review = YearReview { lib_vol: vol, lib_root: Some("사진".into()), keep_edits: "latest".into() };
    let groups = triage::groups_in_with(&conn, "similar_image", &organizer::year_files(&plan, "2018"), &review, 0, 50).unwrap();
    assert_eq!(groups.len(), 1, "{groups:#?}");
    let mut kept: Vec<(String, bool)> = groups[0].members.iter().map(|m| (m.file.name.clone(), m.keep)).collect();
    kept.sort();
    assert_eq!(kept, [("b.jpg".into(), true), ("b_edit1.jpg".into(), false), ("b_edit2.jpg".into(), true), ("b_edit3.jpg".into(), false)]);
    // 2019: the filed original and its one edit — with "latest" nothing to decide.
    assert!(triage::groups_in_with(&conn, "similar_image", &organizer::year_files(&plan, "2019"), &review, 0, 50).unwrap().is_empty());
    // With "none" the edit is offered; the filed original shows as such and stays.
    let none = YearReview { keep_edits: "none".into(), ..review.clone() };
    let g19 = triage::groups_in_with(&conn, "similar_image", &organizer::year_files(&plan, "2019"), &none, 0, 50).unwrap();
    let filed = g19.iter().flat_map(|g| &g.members).find(|m| m.file.name == "a.jpg").unwrap();
    assert!(filed.locked && filed.keep && filed.role.as_deref() == Some("filed"), "{filed:?}");
    // "all": edits stay, locked, as before.
    let all = triage::groups_in_with(&conn, "similar_image", &organizer::year_files(&plan, "2018"), &YearReview { keep_edits: "all".into(), ..review.clone() }, 0, 50).unwrap();
    assert!(all.is_empty() || all[0].members.iter().all(|m| m.keep));

    // (f) Pushed: the scope is in the library, the copy gone, nothing left to do; (e) outside untouched.
    let rep = organizer::execute(&db, &plan, &stop, &quiet).unwrap();
    assert_eq!(rep.failed, 0, "{:?}", rep.errors);
    assert!(lib.join("2018/2018-07/부산/b.jpg").is_file() && lib.join("2019/2019-05/제주/a.jpg").is_file());
    assert!(!e.join("RAID_Recovery/x/f0001.jpg").exists());
    assert!(e.join("옛날/2010-01 서울/c.jpg").is_file());
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    let again = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    assert_eq!((again.summary.moves, again.summary.copies, again.summary.remove), (0, 0, 0));
}

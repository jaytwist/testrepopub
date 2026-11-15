//! Folder actions in the library preview: merge folders (later ones into the first chosen), and
//! delete folders for good (copies on other drives stay, and are not brought back).

use std::fs;
use std::path::Path;

use media_core::library::{self, LibraryOptions, Merge};
use media_core::organizer::{self, Action, OrganizeOptions};
use media_core::scanner::{self, ScanOptions};
use media_core::{volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

fn jpeg(p: &Path, seed: u32, date: &str) {
    write_jpeg(p, &picture(seed, 800, 600), Some((date, "Canon", "EOS R6")));
}

#[test]
fn merge_and_delete_folders() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let c = drive(tmp.path(), "diskC");
    jpeg(&e.join("옛날/2015-08 제주도/j1.jpg"), 1, "2015:08:10 10:00:00");
    jpeg(&e.join("옛날/2015-08 제주도/j3.jpg"), 3, "2015:08:11 10:00:00");
    jpeg(&e.join("옛날/2015-09 제주도 둘째날/j2.jpg"), 2, "2015:09:01 10:00:00");
    jpeg(&e.join("옛날/2016-05 부산/b1.jpg"), 4, "2016:05:01 10:00:00");
    fs::create_dir_all(e.join("백업")).unwrap();
    fs::copy(e.join("옛날/2015-08 제주도/j1.jpg"), e.join("백업/j1.jpg")).unwrap();
    fs::create_dir_all(c.join("Pictures")).unwrap();
    fs::copy(e.join("옛날/2015-08 제주도/j1.jpg"), c.join("Pictures/j1.jpg")).unwrap();
    fs::copy(e.join("옛날/2015-08 제주도/j3.jpg"), c.join("Pictures/j3.jpg")).unwrap();

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    volumes::add_source(&conn, &c).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    let opts = OrganizeOptions {
        target_dir: e.join("사진").to_string_lossy().into_owned(),
        layout: "library".into(),
        library: LibraryOptions::default(),
        ..Default::default()
    };
    let item = |plan: &organizer::Plan, rel: &str| {
        plan.items.iter().find(|i| i.src.replace('\\', "/").ends_with(rel)).cloned().unwrap_or_else(|| panic!("{rel} not planned"))
    };

    // Merge "제주도 둘째날" (picked second) into "제주도" (picked first).
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    let (g1, g2) = (item(&plan, "제주도/j1.jpg").group, item(&plan, "둘째날/j2.jpg").group);
    assert_eq!((g1.as_str(), g2.as_str()), ("2015/2015-08/제주도", "2015/2015-09/제주도 둘째날"));
    library::save_merges(&conn, &[Merge { from: g2.clone(), into: g1.clone() }]).unwrap();
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    assert_eq!(item(&plan, "둘째날/j2.jpg").group, g1, "merged into the first folder");
    assert!(item(&plan, "둘째날/j2.jpg").dst.replace('\\', "/").ends_with("2015/2015-08/제주도/j2.jpg"));
    let rep = organizer::execute(&db, &plan, &stop, &quiet).unwrap();
    assert_eq!(rep.failed, 0, "{:?}", rep.errors);
    let again = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    assert_eq!((again.summary.moves, again.summary.copies), (0, 0), "planning again keeps the merge: {:?}", again.summary);

    // ♥ on j3; then delete the merged folder for good, keeping protected files.
    let j3: i64 = conn.query_row("SELECT id FROM files WHERE rel_path LIKE '%사진%j3.jpg'", [], |r| r.get(0)).unwrap();
    conn.execute("INSERT INTO labels(file_id, label, source) VALUES (?, 'favorite', 'user')", [j3]).unwrap();
    // Put the copy back in 백업 (the run above cleaned it up) so it goes with the folder.
    fs::create_dir_all(e.join("백업")).unwrap();
    fs::copy(e.join("사진/2015/2015-08/제주도/j1.jpg"), e.join("백업/j1.jpg")).unwrap();
    // A photo only on C, from the same trip: planned to be copied into that folder (no twin, so
    // never fingerprinted).
    jpeg(&c.join("여행/2015-08 제주도/c4.jpg"), 5, "2015:08:12 10:00:00");
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    assert_eq!((item(&plan, "제주도/c4.jpg").action, item(&plan, "제주도/c4.jpg").group), (Action::Copy, g1.clone()));
    let t = organizer::delete_targets(&plan, &[g1.clone()]);
    assert_eq!(t.ids.len(), 4, "j1, j2, j3 and j1's copy in 백업: {t:?}");
    let rep = organizer::delete_folders(&db, &plan, &[g1.clone()], true, &stop, &quiet).unwrap();
    assert_eq!((rep.deleted, rep.kept_protected, rep.not_copied), (3, 1, 1), "{rep:?}");
    assert!(!e.join("사진/2015/2015-08/제주도/j1.jpg").exists());
    assert!(!e.join("백업/j1.jpg").exists());
    assert!(e.join("사진/2015/2015-08/제주도/j3.jpg").is_file(), "♥ stays");
    let walked = walk(&e);
    assert!(!walked.iter().any(|p| p.contains(&rep.batch_id)), "gone for good, not left in the quarantine: {walked:?}");
    assert!(!walked.iter().any(|p| p.ends_with("/j2.jpg")), "{walked:?}");
    assert!(c.join("Pictures/j1.jpg").is_file(), "C stays as it is");

    // Next plan: C's j1 is not copied back in; C's j3 is a copy of the kept ♥ photo as before.
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    let cj1 = item(&plan, "Pictures/j1.jpg");
    assert_eq!((cj1.action, cj1.tag.as_str()), (Action::Leave, "excluded"));
    let cc4 = item(&plan, "제주도/c4.jpg");
    assert_eq!((cc4.action, cc4.tag.as_str()), (Action::Leave, "excluded"), "C's own photo of the deleted folder is not copied in either");
    assert_eq!(plan.summary.excluded, 2);
    let cj3 = item(&plan, "Pictures/j3.jpg");
    assert_eq!((cj3.action, cj3.tag.as_str()), (Action::Leave, "copy"));
    assert_eq!(item(&plan, "부산/b1.jpg").action, Action::InPlace, "other folders untouched");
    assert!(e.join("사진/2016/2016-05/부산/b1.jpg").is_file());
}

fn walk(root: &Path) -> Vec<String> {
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

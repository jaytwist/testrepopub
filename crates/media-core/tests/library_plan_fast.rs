//! Planning the library reads nothing from the drives: copies are recognised by the stored
//! fingerprints; files whose fingerprint is not read yet stay put; unreadable files are not read
//! again.

use std::fs;
use std::path::Path;

use media_core::identity;
use media_core::library::LibraryOptions;
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
fn plan_without_the_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    jpeg(&e.join("DCIM/a.jpg"), 1, "2020:01:05 10:00:00");
    jpeg(&e.join("DCIM/b.jpg"), 2, "2020:01:06 10:00:00");
    fs::create_dir_all(e.join("백업")).unwrap();
    fs::copy(e.join("DCIM/a.jpg"), e.join("백업/a.jpg")).unwrap();
    // Already in the library: the same photo as b.jpg under b's name, and a different photo
    // that happens to be called c.jpg where c.jpg wants to go.
    fs::create_dir_all(e.join("사진/2020/2020-01")).unwrap();
    fs::copy(e.join("DCIM/b.jpg"), e.join("사진/2020/2020-01/b.jpg")).unwrap();
    jpeg(&e.join("DCIM/c.jpg"), 3, "2020:01:07 10:00:00");
    jpeg(&e.join("사진/2020/2020-01/c.jpg"), 4, "2020:01:07 11:00:00");

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
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

    // A copy of what is already in the library goes; same name but another photo: numbered.
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    assert_eq!(item(&plan, "백업/a.jpg").action, Action::Remove);
    assert_eq!(item(&plan, "DCIM/b.jpg").action, Action::Remove);
    let c = item(&plan, "DCIM/c.jpg");
    assert!(c.renamed && c.dst.ends_with("c_1.jpg"), "{c:?}");
    assert!(!plan.summary.timing.is_empty());

    // The source folders vanish (drive asleep, say): the plan is the same, since it reads only
    // the index.
    let moved = tmp.path().join("aside");
    fs::rename(e.join("DCIM"), &moved).unwrap();
    let again = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    assert_eq!(again.summary.moves, plan.summary.moves);
    assert_eq!(again.summary.remove, plan.summary.remove);
    fs::rename(&moved, e.join("DCIM")).unwrap();

    // A fingerprint not read yet (indexed by an older version): the copy is not removed, and
    // its twin not moved past it — both wait.
    conn.execute("UPDATE files SET prehash = NULL WHERE rel_path LIKE '%a.jpg'", []).unwrap();
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    assert_eq!(item(&plan, "백업/a.jpg").action, Action::Leave);
    assert_eq!(item(&plan, "백업/a.jpg").tag, "unverified");
    assert_eq!(plan.summary.unverified, 2);
    assert_eq!(plan.summary.remove, 1, "only b.jpg's copy (its fingerprint is known)");
    // Read in the background, then recognised again.
    assert_eq!(identity::backfill_prehashes(&conn).unwrap(), 2);
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    assert_eq!(item(&plan, "백업/a.jpg").action, Action::Remove);

    // A file that cannot be read: marked once, not read again, never a copy of anything.
    conn.execute("UPDATE files SET prehash = NULL WHERE rel_path LIKE '%a.jpg'", []).unwrap();
    fs::remove_file(e.join("백업/a.jpg")).unwrap();
    assert_eq!(identity::backfill_prehashes(&conn).unwrap(), 2);
    let marks: Vec<String> = {
        let mut st = conn.prepare("SELECT prehash FROM files WHERE rel_path LIKE '%a.jpg' ORDER BY rel_path").unwrap();
        let v = st.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        v
    };
    assert!(marks.iter().any(|m| m.starts_with("unreadable-")), "{marks:?}");
    assert_eq!(identity::backfill_prehashes(&conn).unwrap(), 0, "not read again");
}

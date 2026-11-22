//! A work scope (one drive, or one folder with everything below): the library, the cleanup
//! counts, duplicate removal, the review groups, copy folders and damaged files see and change
//! only what is inside. What is outside may be what stays — never what goes.

use std::fs;
use std::path::Path;

use media_core::db::{self, FileFilter, Scope};
use media_core::dedupe::{self, DupKind, Similarity};
use media_core::folders::{self, FolderRef, Pick, RemoveOptions};
use media_core::scanner::{self, ScanOptions};
use media_core::{cleanup, damage, identity, triage, volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

fn jpeg(p: &Path, seed: u32) {
    write_jpeg(p, &picture(seed, 800, 600), Some(("2015:08:10 10:00:00", "Canon", "EOS R6")));
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap().flatten() {
        let (p, t) = (e.path(), to.join(e.file_name()));
        if p.is_dir() {
            copy_tree(&p, &t);
        } else {
            fs::copy(&p, &t).unwrap();
        }
    }
}

#[test]
fn only_inside_is_seen_and_changed() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    // Originals, and a recovery folder deep with copies of them, a copy folder and a damaged file.
    jpeg(&e.join("사진/제주/a.jpg"), 1);
    jpeg(&e.join("사진/제주/b.jpg"), 2);
    jpeg(&e.join("사진/c.jpg"), 3);
    fs::create_dir_all(e.join("RAID_Recovery/x/y")).unwrap();
    fs::copy(e.join("사진/c.jpg"), e.join("RAID_Recovery/x/y/f001.jpg")).unwrap();
    copy_tree(&e.join("사진/제주"), &e.join("RAID_Recovery/제주 복사"));
    let good = fs::read(e.join("사진/c.jpg")).unwrap();
    fs::write(e.join("RAID_Recovery/x/cut.jpg"), &good[..good.len() / 2]).unwrap();
    // A photo inside with its copy also inside: one of the two stays.
    jpeg(&e.join("RAID_Recovery/x/d.jpg"), 4);
    fs::copy(e.join("RAID_Recovery/x/d.jpg"), e.join("RAID_Recovery/x/y/d2.jpg")).unwrap();
    // A sibling whose name only begins the same: not inside.
    fs::create_dir_all(e.join("RAID_Recovery2")).unwrap();
    fs::copy(e.join("사진/c.jpg"), e.join("RAID_Recovery2/c.jpg")).unwrap();

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let mut conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    identity::backfill_prehashes(&conn).unwrap();
    identity::refresh_roles(&mut conn).unwrap();
    let tc = media_core::thumbs::ThumbCache::new(tmp.path().join("cache"));
    media_core::analysis::run(&db, &tc, None, 2, &stop, &media_core::analysis::PauseFlag::default(), &quiet).unwrap();
    dedupe::run(&db, DupKind::Exact, Similarity::High, &stop, &quiet).unwrap();
    let vol: i64 = conn.query_row("SELECT id FROM volumes", [], |r| r.get(0)).unwrap();

    // Chosen and remembered, written any way.
    let scope = Scope::new(vol, "\\RAID_Recovery\\");
    assert_eq!(scope.dir, "RAID_Recovery");
    Scope::save(&conn, Some(&scope)).unwrap();
    assert_eq!(Scope::load(&conn).unwrap(), Some(scope.clone()));

    // (a) The library: the folder and everything below, nothing else; the sibling is outside.
    let f = FileFilter { volume_id: Some(vol), under: Some(scope.dir.clone()), stack_copies: true, ..Default::default() };
    let mut names: Vec<String> = db::query_files(&conn, &f, 0, 100).unwrap().into_iter().map(|r| r.rel_path).collect();
    names.sort();
    assert_eq!(names.len(), 6, "{names:?}");
    assert!(names.iter().all(|n| n.starts_with("RAID_Recovery/")), "{names:?}");
    let tree = db::folder_tree(&conn, vol).unwrap();
    assert!(tree.iter().any(|d| d.dir == "RAID_Recovery/x/y" && d.files == 2), "{tree:?}");

    // (b) The cleanup counts: what is inside only. Every inside copy may go (its original is
    // outside), and one of d / d2.
    let all = cleanup::summary(&conn).unwrap();
    let here = cleanup::summary_in(&conn, Some(&scope)).unwrap();
    assert_eq!(here.damaged.count, 1);
    assert!(all.exact_duplicates.count > here.exact_duplicates.count, "the sibling's copy is not counted: {} / {}", all.exact_duplicates.count, here.exact_duplicates.count);
    assert_eq!(here.exact_duplicates.count, 4, "f001, the two copied trip photos, one of d / d2");

    // (e) Review groups: members outside are locked and stay.
    let groups = triage::groups_scoped(&conn, "exact", None, Some(&scope), 0, 100).unwrap();
    assert_eq!(groups.len(), 4);
    for g in &groups {
        for m in &g.members {
            let outside = !scope.contains(m.file.volume_id, &m.file.rel_path);
            assert_eq!(m.locked && m.role.as_deref() == Some("outside"), outside, "{}", m.file.rel_path);
        }
    }

    // (f) Copy folders: the set is listed for its copy inside; a pick of a folder outside is refused.
    let found = folders::in_scope(folders::find(&conn).unwrap(), Some(&scope));
    assert_eq!(found.sets.len(), 1, "{:#?}", found.sets);
    assert_eq!(found.sets[0].copies[0].rel_dir, "RAID_Recovery/제주 복사");
    let wrong = Pick { keep: FolderRef { volume_id: vol, rel_dir: "RAID_Recovery/제주 복사".into() }, copy: FolderRef { volume_id: vol, rel_dir: "사진/제주".into() }, partial: false };
    let rep = folders::remove_in(&db, &[wrong], &RemoveOptions { purge: false, quick: false }, Some(&scope), &stop, &quiet).unwrap();
    assert_eq!((rep.skipped, rep.folders), (1, 0), "{rep:?}");
    assert!(e.join("사진/제주/a.jpg").is_file());

    // A folder with a Korean name (SQLite counts its characters, not its bytes).
    let ko = FileFilter { volume_id: Some(vol), under: Some("사진".into()), ..Default::default() };
    assert_eq!(db::query_files(&conn, &ko, 0, 100).unwrap().len(), 3);
    let ko2 = FileFilter { volume_id: Some(vol), under: Some("RAID_Recovery/제주 복사".into()), ..Default::default() };
    assert_eq!(db::query_files(&conn, &ko2, 0, 100).unwrap().len(), 2);

    // Damaged files: the one inside.
    assert_eq!(damage::list_in(&conn, Some(&scope), 0, 100).unwrap().total, 1);
    assert_eq!(damage::list_in(&conn, Some(&Scope::new(vol, "사진")), 0, 100).unwrap().total, 0);

    // (c, d) Duplicates removed at once: copies inside go, originals outside stay, one of the
    // pair inside stays, the sibling's copy stays.
    let rep = dedupe::apply_in(&db, "exact", None, Some(&scope), &stop, &quiet).unwrap();
    assert_eq!(rep.quarantined, 4, "{rep:?}");
    for kept in ["사진/c.jpg", "사진/제주/a.jpg", "사진/제주/b.jpg", "RAID_Recovery2/c.jpg"] {
        assert!(e.join(kept).is_file(), "{kept}");
    }
    assert!(!e.join("RAID_Recovery/x/y/f001.jpg").exists());
    assert_eq!(e.join("RAID_Recovery/x/d.jpg").is_file() as u8 + e.join("RAID_Recovery/x/y/d2.jpg").is_file() as u8, 1);
    // The group with the sibling's copy is still open: it is decided when that folder is in scope.
    let open: i64 = conn.query_row("SELECT COUNT(*) FROM dup_groups WHERE kind = 'exact' AND resolved = 0", [], |r| r.get(0)).unwrap();
    assert_eq!(open, 1);

    // Back to everything.
    Scope::save(&conn, None).unwrap();
    assert_eq!(Scope::load(&conn).unwrap(), None);
}

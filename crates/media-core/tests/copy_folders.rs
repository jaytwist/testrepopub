//! "100% 복사 폴더": folders copied whole are found from the index, the oldest stays, and the
//! copies are removed without losing a photo, a mark, or a file that exists only in the copy.

use std::fs;
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use media_core::folders::{self, FolderRef, FolderSet, Pick};
use media_core::rusqlite::params;
use media_core::scanner::{self, ScanOptions};
use media_core::{volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

fn jpeg(p: &Path, seed: u32, date: &str) {
    write_jpeg(p, &picture(seed, 800, 600), Some((date, "Canon", "EOS R6")));
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

fn picks(set: &FolderSet) -> Vec<Pick> {
    let r = |f: &folders::FolderInfo| FolderRef { volume_id: f.volume_id, rel_dir: f.rel_dir.clone() };
    set.copies.iter().map(|c| Pick { keep: r(&set.keep), copy: r(c), partial: false }).collect()
}

fn trip(dir: &Path) {
    jpeg(&dir.join("j1.jpg"), 1, "2015:08:10 10:00:00");
    jpeg(&dir.join("j2.jpg"), 2, "2015:08:10 11:00:00");
    jpeg(&dir.join("둘째날/j3.jpg"), 3, "2015:08:11 10:00:00");
}

#[test]
fn finds_whole_copies_only() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let c = drive(tmp.path(), "diskC");
    let orig = e.join("옛날/2015-08 제주도");
    trip(&orig);
    sleep(Duration::from_millis(30));
    // A copy as it is; a copy with one file renamed; the same folder on another drive.
    copy_tree(&orig, &e.join("백업/제주도 - 복사본"));
    copy_tree(&orig, &e.join("백업2/x"));
    fs::rename(e.join("백업2/x/j1.jpg"), e.join("백업2/x/a.jpg")).unwrap();
    copy_tree(&orig, &c.join("Pictures/제주도"));
    // Not copies: one photo short, one photo more, and a folder that only wraps another.
    fs::create_dir_all(e.join("다른/제주도 일부")).unwrap();
    fs::copy(orig.join("j1.jpg"), e.join("다른/제주도 일부/j1.jpg")).unwrap();
    fs::copy(orig.join("j2.jpg"), e.join("다른/제주도 일부/j2.jpg")).unwrap();
    copy_tree(&orig, &e.join("하나더/제주도"));
    jpeg(&e.join("하나더/제주도/j9.jpg"), 9, "2015:08:12 10:00:00");
    jpeg(&e.join("감싼/안/w1.jpg"), 11, "2016:01:01 10:00:00");
    jpeg(&e.join("감싼/안/w2.jpg"), 12, "2016:01:02 10:00:00");

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    volumes::add_source(&conn, &c).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();

    let found = folders::find(&conn).unwrap();
    // Two sets: the folder with its two copies, and — inside the folder that has one photo
    // more, so is no copy itself — a sub-folder that is one. Sub-folders of the copies, the
    // part copy, the wrapping folder and the other drive are not listed.
    assert_eq!(found.sets.len(), 2, "{:#?}", found.sets);
    let inner = &found.sets[1];
    assert_eq!((inner.keep.rel_dir.as_str(), inner.copies[0].rel_dir.as_str(), inner.files), ("옛날/2015-08 제주도/둘째날", "하나더/제주도/둘째날", 1));
    let s = &found.sets[0];
    assert_eq!(s.keep.rel_dir, "옛날/2015-08 제주도", "the oldest stays");
    assert_eq!(s.keep_why, "oldest");
    // (Which of the two copies comes first depends on which was made first — within the same
    // instant on a fast disk.)
    let mut copies: Vec<&str> = s.copies.iter().map(|c| c.rel_dir.as_str()).collect();
    copies.sort_unstable();
    assert_eq!(copies, ["백업/제주도 - 복사본", "백업2/x"], "the folders themselves, not the folders around them");
    assert_eq!((s.files, s.sample.len()), (3, 3));
    let as_is = s.copies.iter().find(|c| c.rel_dir == "백업/제주도 - 복사본").unwrap();
    assert_eq!(as_is.names, s.keep.names, "copied as it is");
    assert_ne!(s.copies.iter().find(|c| c.rel_dir == "백업2/x").unwrap().names, s.keep.names, "one file was renamed");

    // The user keeps the copy in 백업 instead: the original and the third go.
    let r = |d: &str| FolderRef { volume_id: s.keep.volume_id, rel_dir: d.into() };
    let p = vec![
        Pick { keep: r("백업/제주도 - 복사본"), copy: r("옛날/2015-08 제주도"), partial: false },
        Pick { keep: r("백업/제주도 - 복사본"), copy: r("백업2/x"), partial: false },
    ];
    let rep = folders::remove(&db, &p, true, &stop, &quiet).unwrap();
    assert_eq!((rep.folders, rep.partial, rep.skipped, rep.files, rep.different, rep.left, rep.failed), (2, 0, 0, 6, 0, 0, 0), "{rep:?}");
    assert!(rep.bytes > 0);
    assert!(!orig.exists() && !e.join("백업2/x").exists());
    assert!(!e.join("옛날").exists() && !e.join("백업2").exists(), "the folders that held only the copy go too");
    for f in ["j1.jpg", "j2.jpg", "둘째날/j3.jpg"] {
        assert!(e.join("백업/제주도 - 복사본").join(f).is_file(), "{f} stays");
        assert!(c.join("Pictures/제주도").join(f).is_file(), "C is not touched");
    }
    assert!(!walk(&e).iter().any(|p| p.contains(&rep.batch_id)), "deleted for good, nothing waits in the quarantine");
    let live: i64 = conn.query_row("SELECT COUNT(*) FROM files WHERE missing = 0 AND quarantined = 0 AND volume_id = ?", [s.keep.volume_id], |r| r.get(0)).unwrap();
    assert_eq!(live, 3 + 2 + 4 + 2, "the kept copy, the part copy, the bigger folder, the wrapped one");
    let again = folders::find(&conn).unwrap();
    assert_eq!(again.sets.len(), 1, "{:#?}", again.sets);
    // (Both were made within the same instant here: which of the two stays is not asserted.)
    let mut pair = [again.sets[0].keep.rel_dir.as_str(), again.sets[0].copies[0].rel_dir.as_str()];
    pair.sort_unstable();
    assert_eq!(pair, ["백업/제주도 - 복사본/둘째날", "하나더/제주도/둘째날"]);
    let what: String = conn.query_row("SELECT description FROM batches WHERE id = ?", [&rep.batch_id], |r| r.get(0)).unwrap();
    assert_eq!(what, "100% 복사 폴더 2개 정리");
}

#[test]
fn marks_and_other_files_survive() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let orig = e.join("여행/제주도");
    trip(&orig);
    fs::write(orig.join("메모.txt"), "숙소 전화번호").unwrap();
    fs::write(orig.join("j1.xmp"), "<x:xmpmeta/>").unwrap();
    sleep(Duration::from_millis(30));
    let copy = e.join("백업/제주도 - 복사본");
    copy_tree(&orig, &copy);
    fs::write(copy.join("나만.txt"), "복사본에만 있는 메모").unwrap();
    fs::write(copy.join("둘째날/메모.txt"), "다른 자리의 메모").unwrap();
    fs::write(copy.join("Thumbs.db"), "x").unwrap();

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    // ♥ on the copy's j2 only.
    let cj2: i64 = conn.query_row("SELECT id FROM files WHERE rel_path = '백업/제주도 - 복사본/j2.jpg'", [], |r| r.get(0)).unwrap();
    conn.execute("INSERT INTO labels(file_id, label, source) VALUES (?, 'favorite', 'user')", [cj2]).unwrap();

    let found = folders::find(&conn).unwrap();
    assert_eq!(found.sets.len(), 1, "{:#?}", found.sets);
    assert_eq!(found.sets[0].keep.rel_dir, "여행/제주도");
    let rep = folders::remove(&db, &picks(&found.sets[0]), true, &stop, &quiet).unwrap();
    assert_eq!((rep.folders, rep.partial, rep.files, rep.labels_moved), (0, 1, 3, 1), "{rep:?}");
    assert_eq!((rep.other_deleted, rep.left), (3, 2), "the same memo, the same sidecar and Thumbs.db go; what only the copy has stays: {rep:?}");
    assert!(copy.join("나만.txt").is_file() && copy.join("둘째날/메모.txt").is_file());
    assert!(!copy.join("j1.jpg").exists() && !copy.join("메모.txt").exists() && !copy.join("Thumbs.db").exists());
    assert!(orig.join("메모.txt").is_file() && orig.join("j1.xmp").is_file() && orig.join("둘째날/j3.jpg").is_file());
    let fav: String = conn.query_row("SELECT f.rel_path FROM labels l JOIN files f ON f.id = l.file_id WHERE l.label = 'favorite'", [], |r| r.get(0)).unwrap();
    assert_eq!(fav, "여행/제주도/j2.jpg", "♥ moved to the photo that stays");
}

#[test]
fn a_file_that_only_looks_the_same_is_not_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    // Same size, same first 64 KiB (so the same fingerprint), different at the very end.
    let mut a = vec![5u8; 100_000];
    fs::create_dir_all(e.join("원본")).unwrap();
    fs::create_dir_all(e.join("복사")).unwrap();
    fs::write(e.join("원본/v.jpg"), &a).unwrap();
    fs::write(e.join("원본/w.jpg"), vec![6u8; 90_000]).unwrap();
    sleep(Duration::from_millis(30));
    a[99_999] = 9;
    fs::write(e.join("복사/v.jpg"), &a).unwrap();
    fs::write(e.join("복사/w.jpg"), vec![6u8; 90_000]).unwrap();

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions { min_image_bytes: 0, min_image_side: 0, ..Default::default() }, &stop, &quiet).unwrap();

    let found = folders::find(&conn).unwrap();
    assert_eq!(found.sets.len(), 1, "by fingerprint they are the same: {:#?}", found.sets);
    // Looked at side by side they are "the same" too — until the pair is compared to the end.
    let same_fp = |conn: &media_core::rusqlite::Connection| -> bool {
        conn.query_row("SELECT COUNT(DISTINCT prehash) = 1 FROM files WHERE name = 'v.jpg'", [], |r| r.get(0)).unwrap()
    };
    assert!(same_fp(&conn));
    let rep = folders::remove(&db, &picks(&found.sets[0]), true, &stop, &quiet).unwrap();
    assert_eq!((rep.files, rep.different, rep.folders, rep.partial, rep.failed), (1, 1, 0, 1, 0), "{rep:?}");
    assert!(e.join("원본/v.jpg").is_file() && e.join("원본/w.jpg").is_file());
    assert_eq!(rep.to_review.iter().map(|r| (r.keep.rel_dir.as_str(), r.copy.rel_dir.as_str(), r.files)).collect::<Vec<_>>(), [("원본", "복사", 1)]);
    // The one that differs is back where it was — not hidden in a quarantine — and unchanged.
    assert!(!walk(&e).iter().any(|p| p.contains(".media-organizer-quarantine")), "{:?}", walk(&e));
    assert_eq!(fs::read(e.join("복사/v.jpg")).unwrap()[99_999], 9);
    assert!(!e.join("복사/w.jpg").exists(), "the real copy is gone");
    // From now on the two are different files to everything: no longer a set, and side by
    // side they show as the same photo with different content.
    let fp = |rel: &str| -> String { conn.query_row("SELECT prehash FROM files WHERE rel_path = ?", [rel], |r| r.get(0)).unwrap() };
    assert_ne!(fp("원본/v.jpg"), fp("복사/v.jpg"));
    assert!(folders::find(&conn).unwrap().sets.is_empty());
    let vol: i64 = conn.query_row("SELECT volume_id FROM sources LIMIT 1", [], |r| r.get(0)).unwrap();
    let r = |d: &str| FolderRef { volume_id: vol, rel_dir: d.into() };
    let rows = folders::align(&conn, &r("원본"), &[r("복사")]).unwrap();
    assert_eq!(rows.iter().map(|x| (x.path.as_str(), x.kind.as_str())).collect::<Vec<_>>(), [("v.jpg", "differs"), ("w.jpg", "only")]);
    let ids: Vec<i64> = rows[0].files.iter().flatten().map(|f| f.id).collect();
    let d = folders::first_difference(&conn, ids[0], ids[1]).unwrap();
    assert_eq!((d.at, d.size_a, d.size_b), (Some(99_999), 100_000, 100_000));
}

#[test]
fn changed_or_protected_folders_are_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    trip(&e.join("가/제주도"));
    jpeg(&e.join("나/부산/b1.jpg"), 21, "2016:05:01 10:00:00");
    sleep(Duration::from_millis(30));
    copy_tree(&e.join("가/제주도"), &e.join("가 복사/제주도"));
    copy_tree(&e.join("나/부산"), &e.join("나 복사/부산"));

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    let found = folders::find(&conn).unwrap();
    assert_eq!(found.sets.len(), 2, "{:#?}", found.sets);
    let all: Vec<Pick> = found.sets.iter().flat_map(picks).collect();

    // After listing: a new photo lands in one copy, and the other copy becomes a folder to keep.
    jpeg(&e.join("가 복사/제주도/new.jpg"), 30, "2015:08:12 10:00:00");
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    let rules = format!("{{\"keep\":[{:?}],\"drop\":[]}}", e.join("나 복사").to_string_lossy());
    conn.execute("INSERT OR REPLACE INTO settings(key, value) VALUES ('dup_rules', ?)", params![rules]).unwrap();

    let rep = folders::remove(&db, &all, true, &stop, &quiet).unwrap();
    assert_eq!((rep.skipped, rep.folders, rep.files), (2, 0, 0), "{rep:?}");
    assert!(rep.batch_id.is_empty(), "nothing done, nothing in the history");
    assert!(e.join("가 복사/제주도/j1.jpg").is_file() && e.join("나 복사/부산/b1.jpg").is_file());
    // Listed again: the changed pair is no longer a copy (only its untouched sub-folder is);
    // the folder to keep is the one that stays.
    let found = folders::find(&conn).unwrap();
    let dirs: Vec<(&str, &str)> = found.sets.iter().map(|s| (s.keep.rel_dir.as_str(), s.keep_why.as_str())).collect();
    assert_eq!(dirs, [("나 복사/부산", "keep_rule"), ("가/제주도/둘째날", "oldest")], "{:#?}", found.sets);
    // Without a fingerprint a folder is not judged.
    conn.execute("UPDATE files SET prehash = NULL, hash = NULL WHERE rel_path = '나/부산/b1.jpg'", params![]).unwrap();
    let found = folders::find(&conn).unwrap();
    assert_eq!((found.sets.len(), found.unread), (1, 1), "{:#?}", found.sets);
    // A file nothing else has the size of is never read, and is not waited for.
    conn.execute("UPDATE files SET prehash = NULL, hash = NULL WHERE rel_path = '가 복사/제주도/new.jpg'", params![]).unwrap();
    assert_eq!(folders::find(&conn).unwrap().unread, 1);
    assert_eq!(found.sets[0].keep.rel_dir, "가/제주도/둘째날");
}

#[test]
fn only_the_library_drive_once_one_is_chosen() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let c = drive(tmp.path(), "diskC");
    for d in [&e, &c] {
        trip(&d.join("여행/제주도"));
        sleep(Duration::from_millis(30));
        copy_tree(&d.join("여행/제주도"), &d.join("백업/제주도"));
    }
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    volumes::add_source(&conn, &c).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    // No library yet: every drive's own copies are listed.
    let all = folders::find(&conn).unwrap();
    assert_eq!((all.sets.len(), all.drive.is_none()), (2, true), "{:#?}", all.sets);
    let on_c: Vec<Pick> = all.sets.iter().filter(|s| s.keep.abs_path.contains("diskC")).flat_map(picks).collect();
    assert_eq!(on_c.len(), 1);

    // The library is on E: C is neither listed nor touched, even when asked.
    media_core::importer::ImportSettings { target_dir: e.join("사진").to_string_lossy().into_owned(), ..Default::default() }.save(&conn).unwrap();
    let found = folders::find(&conn).unwrap();
    assert_eq!(found.sets.len(), 1, "{:#?}", found.sets);
    assert!(found.sets[0].keep.abs_path.contains("diskE") && found.drive.is_some());
    let rep = folders::remove(&db, &on_c, true, &stop, &quiet).unwrap();
    assert_eq!((rep.skipped, rep.files), (1, 0), "{rep:?}");
    assert!(c.join("백업/제주도/j1.jpg").is_file());
    let rep = folders::remove(&db, &picks(&found.sets[0]), true, &stop, &quiet).unwrap();
    assert_eq!((rep.folders, rep.files), (1, 3), "{rep:?}");
    assert!(!e.join("백업/제주도").exists() && e.join("여행/제주도/둘째날/j3.jpg").is_file());
}

/// A file the scanner indexes (as a photo) without it being a real picture: quick to make.
fn blob(p: &Path, seed: u32) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, (0..4000 + seed * 7).map(|i| (i * (seed + 3)) as u8).collect::<Vec<u8>>()).unwrap();
}

#[test]
fn almost_the_same_folders_are_listed_apart() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    // 30 photos: 20 in the folder, 10 in a sub-folder.
    let a = e.join("여행/유럽");
    for i in 0..20 {
        blob(&a.join(format!("a{i:02}.jpg")), i);
    }
    for i in 0..10 {
        blob(&a.join(format!("파리/p{i}.jpg")), 100 + i);
    }
    sleep(Duration::from_millis(30));
    // B: one photo missing, one of its own (29 of 30 the same). C: three missing, one of its own (27 of 30).
    let b = e.join("백업/유럽 사본");
    copy_tree(&a, &b);
    fs::remove_file(b.join("a19.jpg")).unwrap();
    blob(&b.join("b_only.jpg"), 200);
    fs::write(b.join("메모.txt"), "복사본에만 있는 메모").unwrap();
    let c = e.join("다른/유럽 조금");
    copy_tree(&a, &c);
    for n in ["a17.jpg", "a18.jpg", "a19.jpg"] {
        fs::remove_file(c.join(n)).unwrap();
    }
    blob(&c.join("c_only.jpg"), 201); // (with nothing of its own it would be listed as contained)

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions { min_image_bytes: 0, min_image_side: 0, ..Default::default() }, &stop, &quiet).unwrap();

    let found = folders::find(&conn).unwrap();
    // 100 %: only the sub-folder, which all three have unchanged.
    assert_eq!(found.sets.len(), 1, "{:#?}", found.sets);
    assert_eq!((found.sets[0].keep.rel_dir.as_str(), found.sets[0].copies.len()), ("여행/유럽/파리", 2));
    // 95 %: A and B (C is 90 %); the sub-folders are not listed again.
    assert_eq!(found.near.len(), 1, "{:#?}", found.near);
    let n = &found.near[0];
    assert_eq!((n.base.rel_dir.as_str(), n.others.len()), ("여행/유럽", 1), "the bigger — here the older — is the base");
    let o = &n.others[0];
    assert_eq!((o.info.rel_dir.as_str(), o.shared, o.only_base, o.only_here), ("백업/유럽 사본", 29, 1, 1));
    assert!(o.ratio > 0.96 && o.ratio < 0.97 && o.shared_bytes > 0);
    // A lower bar lists more: at 90 % C joins (27 of 30), at 95 % it does not.
    let at90 = folders::find_with(&conn, 0.90).unwrap();
    assert_eq!(at90.near.len(), 1, "{:#?}", at90.near);
    assert_eq!(at90.near[0].others.iter().map(|o| (o.info.rel_dir.as_str(), o.shared)).collect::<Vec<_>>(), [("백업/유럽 사본", 29), ("다른/유럽 조금", 27)]);
    assert_eq!(at90.near[0].others[1].only_here, 1);
    // What the cards show.
    assert_eq!((n.base.photos, n.base.videos, n.base.subfolders), (30, 0, 1));
    assert_eq!(n.base.bytes, (0..20).map(|i| 4000 + i * 7).chain((100..110).map(|i| 4000 + i * 7)).sum::<i64>());
    assert!(n.base.created.is_some() || n.base.modified.is_some());

    // Looking inside.
    let r = |d: &str| FolderRef { volume_id: n.base.volume_id, rel_dir: d.into() };
    let inside = folders::files_in(&conn, &r("여행/유럽")).unwrap();
    assert_eq!(inside.iter().map(|f| (f.dir.as_str(), f.ids.len())).collect::<Vec<_>>(), [("", 20), ("파리", 10)]);
    let cmp = folders::compare(&conn, &r("여행/유럽"), &r("백업/유럽 사본")).unwrap();
    assert_eq!((cmp.shared.len(), cmp.only_keep.len(), cmp.only_copy.len()), (29, 1, 1));
    let name = |id: i64| -> String { conn.query_row("SELECT name FROM files WHERE id = ?", [id], |r| r.get(0)).unwrap() };
    assert_eq!((name(cmp.only_keep[0]), name(cmp.only_copy[0])), ("a19.jpg".to_string(), "b_only.jpg".to_string()));
    assert!(cmp.shared.iter().all(|(k, c)| name(*k) == name(*c)), "pairs are the files at the same place");
    let props = folders::props(&conn, &r("백업/유럽 사본")).unwrap();
    assert_eq!((props.other_files, props.examples.as_slice()), (1, ["메모.txt".to_string()].as_slice()));

    // As a 100 % copy it is refused; as an almost-copy only what both have goes.
    let whole = Pick { keep: r("여행/유럽"), copy: r("백업/유럽 사본"), partial: false };
    let rep = folders::remove(&db, &[whole], true, &stop, &quiet).unwrap();
    assert_eq!((rep.skipped, rep.files), (1, 0), "{rep:?}");
    let part = Pick { keep: r("여행/유럽"), copy: r("백업/유럽 사본"), partial: true };
    let rep = folders::remove(&db, &[part], true, &stop, &quiet).unwrap();
    assert_eq!((rep.files, rep.unique_left, rep.folders, rep.partial, rep.different, rep.failed), (29, 1, 0, 1, 0, 0), "{rep:?}");
    assert_eq!(rep.left, 1, "the memo only the copy has: {rep:?}");
    assert!(b.join("b_only.jpg").is_file() && b.join("메모.txt").is_file(), "what only the copy has stays where it is");
    assert!(!b.join("a00.jpg").exists() && !b.join("파리").exists());
    assert_eq!(walk(&a).len(), 30, "the base folder is untouched");
    assert!(!walk(&e).iter().any(|p| p.contains(&rep.batch_id)));
    let what: String = conn.query_row("SELECT description FROM batches WHERE id = ?", [&rep.batch_id], |r| r.get(0)).unwrap();
    assert_eq!(what, "거의 같은 폴더 1개에서 겹치는 사진 정리");
    let found = folders::find(&conn).unwrap();
    assert!(found.near.is_empty(), "{:#?}", found.near);
    assert_eq!(found.sets.len(), 1, "A/파리 and C/파리 are still a set");
}

#[test]
fn side_by_side_rows_and_taking_the_better_file() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let a = e.join("원본");
    for i in 0..6 {
        blob(&a.join(format!("a{i:02}.jpg")), i);
    }
    blob(&a.join("하위/b0.jpg"), 20);
    sleep(Duration::from_millis(30));
    // 복사: a01 is another file under the same name, a05 is missing, and it has one of its own.
    // 복사2: an exact copy.
    let b = e.join("복사");
    copy_tree(&a, &b);
    blob(&b.join("a01.jpg"), 50);
    fs::remove_file(b.join("a05.jpg")).unwrap();
    blob(&b.join("only.jpg"), 60);
    copy_tree(&a, &e.join("복사2"));

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions { min_image_bytes: 0, min_image_side: 0, ..Default::default() }, &stop, &quiet).unwrap();
    let vol: i64 = conn.query_row("SELECT volume_id FROM sources LIMIT 1", [], |r| r.get(0)).unwrap();
    let r = |d: &str| FolderRef { volume_id: vol, rel_dir: d.into() };
    let id = |rel: &str| -> i64 { conn.query_row("SELECT id FROM files WHERE rel_path = ? AND quarantined = 0", [rel], |r| r.get(0)).unwrap() };
    // The original's a01 could not be opened by the analysis, and carries a ♥.
    conn.execute("INSERT INTO analysis(file_id, version, analyzed_at, face_version) VALUES (?, 1, 0, 0)", [id("원본/a01.jpg")]).unwrap();
    conn.execute("INSERT INTO labels(file_id, label, source) VALUES (?, 'favorite', 'user')", [id("원본/a01.jpg")]).unwrap();

    // Photo by photo across the three folders.
    let rows = folders::align(&conn, &r("원본"), &[r("복사"), r("복사2")]).unwrap();
    let kinds: Vec<(&str, &str)> = rows.iter().map(|x| (x.path.as_str(), x.kind.as_str())).collect();
    assert_eq!(
        kinds,
        [("a00.jpg", "same"), ("a01.jpg", "differs"), ("a02.jpg", "same"), ("a03.jpg", "same"), ("a04.jpg", "same"), ("a05.jpg", "only"), ("only.jpg", "only"), ("하위/b0.jpg", "same")]
    );
    let a01 = &rows[1];
    assert_eq!(a01.files.iter().map(|f| f.as_ref().map(|f| (f.same, f.unreadable))).collect::<Vec<_>>(), [Some((true, true)), Some((false, false)), Some((true, true))]);
    assert_eq!(rows[5].files.iter().map(|f| f.is_some()).collect::<Vec<_>>(), [true, false, true], "a05: not in 복사");
    assert_eq!(rows[6].files.iter().map(|f| f.is_some()).collect::<Vec<_>>(), [false, true, false], "only.jpg: only in 복사");

    // In the list: 5 of 7 are the same and one more is the same photo with other content — the
    // pair counts as 6 of 7 alike, and says which kind of difference it has.
    let near = folders::find_with(&conn, 0.8).unwrap().near;
    assert_eq!(near.len(), 1, "{near:#?}");
    let o = &near[0].others[0];
    assert_eq!((near[0].base.rel_dir.as_str(), o.info.rel_dir.as_str()), ("원본", "복사"));
    assert_eq!((o.shared, o.differs, o.only_base, o.only_here), (5, 1, 1, 1));
    assert!(o.ratio > 0.85 && o.ratio < 0.86);
    assert_eq!(near[0].base.unreadable, 1);
    // The copy's a01 is the good one: it replaces the original's; and only.jpg comes along.
    let old = fs::read(a.join("a01.jpg")).unwrap();
    let good = fs::read(b.join("a01.jpg")).unwrap();
    let choices = vec![
        folders::Choice { keep_file: Some(id("원본/a01.jpg")), copy_file: id("복사/a01.jpg"), discard: false },
        folders::Choice { keep_file: None, copy_file: id("복사/only.jpg"), discard: false },
    ];
    let groups = vec![folders::SwapGroup { copy: r("복사"), choices }];
    let rep = folders::swap(&db, &r("원본"), &groups, &stop, &quiet).unwrap();
    assert_eq!((rep.replaced, rep.brought, rep.failed), (1, 1, 0), "{rep:?}");
    assert_eq!(fs::read(a.join("a01.jpg")).unwrap(), good, "the original's place holds the copy's file");
    assert!(a.join("only.jpg").is_file() && b.join("a01.jpg").is_file() && b.join("only.jpg").is_file(), "the copy folder is as it was");
    let waiting: Vec<String> = walk(&e).into_iter().filter(|p| p.contains(&rep.batch_id)).collect();
    assert_eq!(waiting.len(), 1, "the replaced file waits in the quarantine: {waiting:?}");
    assert_eq!(fs::read(&waiting[0]).unwrap(), old);
    let fav: String = conn.query_row("SELECT f.rel_path FROM labels l JOIN files f ON f.id = l.file_id WHERE l.label = 'favorite' AND f.quarantined = 0", [], |r| r.get(0)).unwrap();
    assert_eq!(fav, "원본/a01.jpg", "♥ is on the new file");
    let what: String = conn.query_row("SELECT description FROM batches WHERE id = ?", [&rep.batch_id], |r| r.get(0)).unwrap();
    assert_eq!(what, "사진 1장을 복사본 것으로 교체, 1장을 원본 폴더로 가져옴");
    let rows = folders::align(&conn, &r("원본"), &[r("복사")]).unwrap();
    assert!(rows.iter().all(|x| x.kind == "same" || x.path == "a05.jpg"), "{rows:#?}");

    // Undone while the copy is still there: everything as before.
    let undo = media_core::journal::undo_batch(&conn, &rep.batch_id).unwrap();
    assert_eq!(undo.failed, 0, "{undo:?}");
    assert_eq!(fs::read(a.join("a01.jpg")).unwrap(), old);
    assert!(!a.join("only.jpg").exists());
    assert_eq!(id("원본/a01.jpg"), groups[0].choices[0].keep_file.unwrap(), "the old row has its place again");

    // Done again, then the copy folder goes (what both have): now the swap cannot be undone —
    // the new file is the only good one, and it stays.
    let rep = folders::swap(&db, &r("원본"), &groups, &stop, &quiet).unwrap();
    assert_eq!((rep.replaced, rep.brought), (1, 1), "{rep:?}");
    let gone = folders::remove(&db, &[Pick { keep: r("원본"), copy: r("복사"), partial: true }], true, &stop, &quiet).unwrap();
    assert_eq!((gone.files, gone.unique_left, gone.folders), (7, 0, 1), "{gone:?}");
    let undo = media_core::journal::undo_batch(&conn, &rep.batch_id).unwrap();
    assert!(undo.failed >= 2, "{undo:?}");
    assert_eq!(fs::read(a.join("a01.jpg")).unwrap(), good);
    assert!(a.join("only.jpg").is_file());
    // The marks of sets looked through are kept.
    assert_eq!(folders::set_reviewed(&conn, "1:원본", true).unwrap(), ["1:원본"]);
    assert!(folders::set_reviewed(&conn, "1:원본", false).unwrap().is_empty());
}

#[test]
fn a_pair_compared_to_the_end_is_known_as_different_at_once() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let mut data = vec![5u8; 100_000];
    fs::create_dir_all(e.join("원본")).unwrap();
    fs::create_dir_all(e.join("복사")).unwrap();
    fs::write(e.join("원본/v.jpg"), &data).unwrap();
    fs::write(e.join("원본/w.jpg"), vec![6u8; 90_000]).unwrap();
    sleep(Duration::from_millis(30));
    data[70_000] = 9;
    fs::write(e.join("복사/v.jpg"), &data).unwrap();
    fs::write(e.join("복사/w.jpg"), vec![6u8; 90_000]).unwrap();
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    scanner::scan_all(&db, &ScanOptions { min_image_bytes: 0, min_image_side: 0, ..Default::default() }, &StopFlag::new(), &quiet).unwrap();
    let vol: i64 = conn.query_row("SELECT volume_id FROM sources LIMIT 1", [], |r| r.get(0)).unwrap();
    let r = |d: &str| FolderRef { volume_id: vol, rel_dir: d.into() };
    let rows = folders::align(&conn, &r("원본"), &[r("복사")]).unwrap();
    assert!(rows.iter().all(|x| x.kind == "same"), "by fingerprint: {rows:#?}");
    assert_eq!(folders::find(&conn).unwrap().sets.len(), 1);
    // A photo cannot be discarded without the original's file that stays.
    let lone = folders::Choice { keep_file: None, copy_file: rows[0].files[1].as_ref().unwrap().id, discard: true };
    let rep = folders::swap(&db, &r("원본"), &[folders::SwapGroup { copy: r("복사"), choices: vec![lone] }], &StopFlag::new(), &quiet).unwrap();
    assert_eq!((rep.discarded, rep.failed), (0, 1), "{rep:?}");
    // Checked: w is the same to the end, v is not — and the index knows from then on.
    let pair = |n: usize| -> (i64, i64) { (rows[n].files[0].as_ref().unwrap().id, rows[n].files[1].as_ref().unwrap().id) };
    assert_eq!(folders::check_pair(&conn, pair(1).0, pair(1).1).unwrap().at, None);
    assert_eq!(folders::check_pair(&conn, pair(0).0, pair(0).1).unwrap().at, Some(70_000));
    let rows = folders::align(&conn, &r("원본"), &[r("복사")]).unwrap();
    assert_eq!(rows.iter().map(|x| x.kind.as_str()).collect::<Vec<_>>(), ["differs", "same"]);
    assert!(folders::find(&conn).unwrap().sets.is_empty(), "no longer a 100 % copy");
    // The original's v is the good one: the copy's goes to the quarantine (and can come back).
    let choice = folders::Choice { keep_file: Some(pair(0).0), copy_file: pair(0).1, discard: true };
    let rep = folders::swap(&db, &r("원본"), &[folders::SwapGroup { copy: r("복사"), choices: vec![choice] }], &StopFlag::new(), &quiet).unwrap();
    assert_eq!((rep.discarded, rep.replaced, rep.failed), (1, 0, 0), "{rep:?}");
    assert!(!e.join("복사/v.jpg").exists() && e.join("원본/v.jpg").is_file());
    let undo = media_core::journal::undo_batch(&conn, &rep.batch_id).unwrap();
    assert_eq!((undo.restored, undo.failed), (1, 0));
    assert_eq!(fs::read(e.join("복사/v.jpg")).unwrap()[70_000], 9, "it can come back");
    // Discarded again: everything left in the copy folder is in the original. Such a folder is
    // listed however small a part of the original it is.
    let choice = folders::Choice { keep_file: Some(pair(0).0), copy_file: pair(0).1, discard: true };
    folders::swap(&db, &r("원본"), &[folders::SwapGroup { copy: r("복사"), choices: vec![choice] }], &StopFlag::new(), &quiet).unwrap();
    blob(&e.join("원본/x1.jpg"), 71);
    blob(&e.join("원본/x2.jpg"), 72);
    blob(&e.join("복사/w2.jpg"), 73);
    blob(&e.join("복사/w3.jpg"), 74);
    fs::copy(e.join("복사/w2.jpg"), e.join("원본/w2.jpg")).unwrap();
    fs::copy(e.join("복사/w3.jpg"), e.join("원본/w3.jpg")).unwrap();
    scanner::scan_all(&db, &ScanOptions { min_image_bytes: 0, min_image_side: 0, ..Default::default() }, &StopFlag::new(), &quiet).unwrap();
    let near = folders::find(&conn).unwrap().near;
    assert_eq!(near.len(), 1, "3 of the original's 6 — far from 95 %, but all of the copy: {near:#?}");
    let o = &near[0].others[0];
    assert_eq!((near[0].base.rel_dir.as_str(), o.info.rel_dir.as_str(), o.shared, o.only_here, o.differs), ("원본", "복사", 3, 0, 0));
    assert!(o.ratio < 0.51);
    let gone = folders::remove(&db, &[Pick { keep: r("원본"), copy: r("복사"), partial: true }], true, &StopFlag::new(), &quiet).unwrap();
    assert_eq!((gone.files, gone.unique_left, gone.folders, gone.different), (3, 0, 1, 0), "{gone:?}");
    assert_eq!(walk(&e.join("원본")).len(), 6);
}

#[test]
fn of_two_differing_pictures_the_damaged_one_is_told() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    jpeg(&e.join("원본/p.jpg"), 1, "2020:01:01 10:00:00");
    // The copy: the same picture, its second half overwritten (same size).
    let mut bytes = fs::read(e.join("원본/p.jpg")).unwrap();
    let half = bytes.len() / 2;
    bytes[half..].fill(0);
    fs::create_dir_all(e.join("복사")).unwrap();
    fs::write(e.join("복사/p.jpg"), &bytes).unwrap();
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    scanner::scan_all(&db, &ScanOptions { min_image_bytes: 0, min_image_side: 0, ..Default::default() }, &StopFlag::new(), &quiet).unwrap();
    let id = |rel: &str| -> i64 { conn.query_row("SELECT id FROM files WHERE rel_path = ?", [rel], |r| r.get(0)).unwrap() };
    let d = folders::check_pair(&conn, id("원본/p.jpg"), id("복사/p.jpg")).unwrap();
    assert_eq!(d.at, Some(half as u64));
    assert_eq!((d.whole_a, d.whole_b), (Some(true), Some(false)), "the copy has an empty stretch the original does not");
    // Seen from the other side it is the same file that is damaged.
    let back = folders::check_pair(&conn, id("복사/p.jpg"), id("원본/p.jpg")).unwrap();
    assert_eq!((back.whole_a, back.whole_b), (Some(false), Some(true)));
    // Two files that merely differ (a byte changed): nothing to tell from the bytes.
    let mut edited = fs::read(e.join("원본/p.jpg")).unwrap();
    edited[half] ^= 0xFF;
    fs::write(e.join("복사/p.jpg"), &edited).unwrap();
    let d = folders::check_pair(&conn, id("원본/p.jpg"), id("복사/p.jpg")).unwrap();
    assert_eq!((d.at, d.whole_a, d.whole_b), (Some(half as u64), None, None));
    // Cut short instead of overwritten: no end marker.
    fs::write(e.join("복사/p.jpg"), &fs::read(e.join("원본/p.jpg")).unwrap()[..half]).unwrap();
    let d = folders::check_pair(&conn, id("원본/p.jpg"), id("복사/p.jpg")).unwrap();
    assert_eq!((d.at, d.whole_a, d.whole_b), (Some(half as u64), Some(true), Some(false)));
}

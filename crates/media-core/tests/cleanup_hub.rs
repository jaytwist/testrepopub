//! Cleanup hub foundations: protection on every delete, backups on other drives are kept, variant
//! kinds of similar photos.

use std::fs;
use std::path::{Path, PathBuf};

use media_core::dedupe::{self, DupKind, DupRules, Similarity};
use media_core::scanner::{self, ScanOptions};
use media_core::{cleanup, db, identity, journal, protect, volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

struct Lib {
    _tmp: tempfile::TempDir,
    db: Db,
    a: PathBuf,
    b: PathBuf,
}

/// Drive A: a photo and a second copy of it, a ♥ photo, a lone screenshot, a photo in a keep
/// folder. Drive B: a backup copy of the photo.
fn lib() -> Lib {
    let tmp = tempfile::tempdir().unwrap();
    let a = drive(tmp.path(), "diskA");
    let b = drive(tmp.path(), "diskB");
    let img = picture(7, 640, 480);
    write_jpeg(&a.join("Pictures/a.jpg"), &img, Some(("2021:05:01 10:00:00", "Canon", "EOS")));
    fs::create_dir_all(a.join("Pictures/copy")).unwrap();
    fs::copy(a.join("Pictures/a.jpg"), a.join("Pictures/copy/a.jpg")).unwrap();
    fs::create_dir_all(b.join("backup")).unwrap();
    fs::copy(a.join("Pictures/a.jpg"), b.join("backup/a.jpg")).unwrap();
    write_jpeg(&a.join("Pictures/fav.jpg"), &picture(8, 640, 480), Some(("2021:05:02 10:00:00", "Canon", "EOS")));
    write_png(&a.join("Screenshots/Screenshot_20230101.png"), &picture(9, 1080, 2340));
    write_jpeg(&a.join("Keep/k.jpg"), &picture(10, 640, 480), Some(("2021:05:03 10:00:00", "Canon", "EOS")));
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    {
        let conn = db.conn().unwrap();
        volumes::add_source(&conn, &a).unwrap();
        volumes::add_source(&conn, &b).unwrap();
    }
    scanner::scan_all(&db, &ScanOptions::default(), &StopFlag::new(), &quiet).unwrap();
    Lib { _tmp: tmp, db, a, b }
}

impl Lib {
    /// The indexed file at `rel` on the drive at `root`.
    fn id(&self, root: &Path, rel: &str) -> i64 {
        let conn = self.db.conn().unwrap();
        let mut st = conn.prepare("SELECT f.id, f.rel_path, v.mount FROM files f JOIN volumes v ON v.id = f.volume_id").unwrap();
        let rows: Vec<(i64, String, String)> =
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().collect::<Result<_, _>>().unwrap();
        let vol = root.file_name().unwrap().to_str().unwrap();
        rows.into_iter()
            .find(|(_, p, m)| p.replace('\\', "/") == rel && m.replace('\\', "/").trim_end_matches('/').ends_with(vol))
            .unwrap_or_else(|| panic!("{rel} not indexed"))
            .0
    }
    fn label(&self, id: i64, label: &str) {
        self.db.conn().unwrap().execute("INSERT INTO labels(file_id, label, source) VALUES (?, ?, 'user')", media_core::rusqlite::params![id, label]).unwrap();
    }
    fn reasons(&self, ids: &[i64]) -> Vec<(i64, String)> {
        protect::check(&self.db.conn().unwrap(), ids).unwrap().into_iter().map(|b| (b.id, b.reason)).collect()
    }
}

#[test]
fn protection_applies_to_every_delete() {
    let l = lib();
    let a = l.id(&l.a, "Pictures/a.jpg");
    let a_copy = l.id(&l.a, "Pictures/copy/a.jpg");
    let backup = l.id(&l.b, "backup/a.jpg");
    let fav = l.id(&l.a, "Pictures/fav.jpg");
    let shot = l.id(&l.a, "Screenshots/Screenshot_20230101.png");
    let kept = l.id(&l.a, "Keep/k.jpg");
    // Content keys are known for the three copies (same size → prehash stored).
    dedupe::run(&l.db, DupKind::Exact, Similarity::High, &StopFlag::new(), &quiet).unwrap();

    l.label(fav, "favorite");
    DupRules { keep: vec![l.a.join("Keep").to_string_lossy().into_owned()], drop: vec![] }.save(&l.db.conn().unwrap()).unwrap();

    assert_eq!(l.reasons(&[fav]), vec![(fav, "favorite".into())]);
    assert_eq!(l.reasons(&[kept]), vec![(kept, "keep_folder".into())]);
    assert!(l.reasons(&[shot]).is_empty(), "a lone file is not a 'last copy'");
    assert!(l.reasons(&[a, a_copy]).is_empty(), "the backup on drive B survives");
    let all = l.reasons(&[a, a_copy, backup]);
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].1, "last_copy");

    l.label(a_copy, "original");
    assert_eq!(l.reasons(&[a_copy]), vec![(a_copy, "original".into())]);

    // Deleting leaves protected files in place unless forced.
    let stop = StopFlag::new();
    let rep = journal::delete_files(&l.db, &[fav, shot, kept], false, Some("테스트"), &stop, &quiet).unwrap();
    assert_eq!((rep.deleted, rep.protected), (1, 2), "{rep:?}");
    assert!(l.a.join("Pictures/fav.jpg").is_file());
    assert!(!l.a.join("Screenshots/Screenshot_20230101.png").exists());
    let rep = journal::delete_files(&l.db, &[fav], true, None, &stop, &quiet).unwrap();
    assert_eq!((rep.deleted, rep.protected), (1, 0), "{rep:?}");
    assert!(!l.a.join("Pictures/fav.jpg").exists());
}

#[test]
fn backups_on_other_drives_are_kept() {
    let l = lib();
    let a = l.id(&l.a, "Pictures/a.jpg");
    let a_copy = l.id(&l.a, "Pictures/copy/a.jpg");
    let backup = l.id(&l.b, "backup/a.jpg");
    let stop = StopFlag::new();
    dedupe::run(&l.db, DupKind::Exact, Similarity::High, &stop, &quiet).unwrap();
    let mut conn = l.db.conn().unwrap();
    let g = &dedupe::list_groups(&conn, "exact", 0, 10).unwrap()[0];
    let kept: Vec<i64> = g.members.iter().filter(|m| m.keep).map(|m| m.file.id).collect();
    assert_eq!(g.members.len(), 3);
    assert_eq!(kept.len(), 2, "one per drive: {kept:?}");
    assert!(kept.contains(&backup));
    assert_eq!(cleanup::summary(&conn).unwrap().exact_duplicates.count, 1);

    identity::refresh_roles(&mut conn).unwrap();
    // One original; the best copy on the other drive is its backup; the extra copy on the same
    // drive as one of them is a plain copy.
    let role = |id: i64| -> String { conn.query_row("SELECT role FROM dup_roles WHERE file_id = ?", [id], |r| r.get(0)).unwrap() };
    let mut roles = vec![role(a), role(a_copy), role(backup)];
    roles.sort();
    assert_eq!(roles, vec!["backup".to_string(), "copy".to_string(), "original".to_string()]);
    assert_eq!(role(a) == "copy" || role(a_copy) == "copy", true, "the extra copy is on drive A");
    assert_eq!(cleanup::summary(&conn).unwrap().backups.count, 1);

    // Applying removes only the extra copy on drive A.
    let rep = dedupe::apply(&l.db, "exact", None, &stop, &quiet).unwrap();
    assert_eq!(rep.quarantined, 1, "{rep:?}");
    assert!(l.b.join("backup/a.jpg").is_file());
    // One copy per drive left: nothing to remove any more.
    dedupe::run(&l.db, DupKind::Exact, Similarity::High, &stop, &quiet).unwrap();
    assert_eq!(cleanup::summary(&conn).unwrap().exact_duplicates.count, 0);
}

#[test]
fn variant_kinds_from_stored_values() {
    let l = lib();
    let conn = l.db.conn().unwrap();
    let best = db::get_file(&conn, l.id(&l.a, "Pictures/a.jpg")).unwrap().unwrap();
    let with = |w: i64, h: i64, size: i64| {
        let mut f = best.clone();
        (f.width, f.height, f.size) = (Some(w), Some(h), size);
        f
    };
    let (w, h, s) = (best.width.unwrap(), best.height.unwrap(), best.size);
    assert_eq!(dedupe::variant_kind(&best, &with(w / 2, h / 2, s / 3)), "smaller");
    assert_eq!(dedupe::variant_kind(&best, &with(w, h, s - 100)), "recompressed");
    assert_eq!(dedupe::variant_kind(&best, &with(w, h * 3 / 4, s)), "edited", "cropped");
    assert_eq!(dedupe::variant_kind(&best, &with(h, w, s)), "edited", "rotated");
    // Same look, but both taken (by the camera clock) years apart: two photos, not a copy.
    let mut other = with(w, h, s - 100);
    other.taken_at = best.taken_at.map(|t| t - 7 * 365 * 86_400);
    assert_eq!(dedupe::variant_kind(&best, &other), "edited", "different moment");
}

#[test]
fn accidental_short_videos() {
    let l = lib();
    let conn = l.db.conn().unwrap();
    for (name, ms) in [("clip.mp4", 1_500), ("movie.mp4", 60_000)] {
        conn.execute(
            "INSERT INTO files(volume_id, rel_path, name, ext, kind, size, mtime, duration_ms) VALUES (1, ?1, ?1, 'mp4', 'video', 1000, 0, ?2)",
            media_core::rusqlite::params![name, ms],
        )
        .unwrap();
    }
    assert_eq!(cleanup::summary(&conn).unwrap().short_videos.count, 1);
    let ids = db::query_ids(&conn, &db::FileFilter { suggest: Some("short_video".into()), ..Default::default() }).unwrap();
    assert_eq!(ids.len(), 1);
}

#[test]
fn quick_review_groups_and_decisions() {
    let l = lib();
    dedupe::run(&l.db, DupKind::Exact, Similarity::High, &StopFlag::new(), &quiet).unwrap();
    let mut conn = l.db.conn().unwrap();
    let gs = media_core::triage::groups(&conn, "exact", None, 0, 10).unwrap();
    assert_eq!(gs.len(), 1);
    let g = &gs[0];
    // The best copy on the other drive is a locked backup.
    let b = g.members.iter().find(|m| m.role.as_deref() == Some("backup")).expect("a backup");
    assert!(b.locked && b.keep, "{b:?}");
    assert_ne!(b.file.volume_id, g.members[0].file.volume_id);
    assert_eq!(g.members.iter().filter(|m| !m.keep).count(), 1, "one extra copy on the same drive");

    // Keeping everything settles the group: it is not asked again.
    let all: Vec<i64> = g.members.iter().map(|m| m.file.id).collect();
    media_core::triage::commit(&mut conn, &[media_core::triage::Decision { key: g.key.clone(), keep: all }]).unwrap();
    assert!(media_core::triage::groups(&conn, "exact", None, 0, 10).unwrap().is_empty());
}

#[test]
fn content_lookups_use_the_index() {
    let l = lib();
    let conn = l.db.conn().unwrap();
    // The same query `protect` runs for every content key: an index search, not a table scan.
    let plan: Vec<String> = conn
        .prepare("EXPLAIN QUERY PLAN SELECT COUNT(*) FROM files WHERE COALESCE(prehash, hash) = ?1 AND missing = 0 AND quarantined = 0")
        .unwrap()
        .query_map(["x"], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(plan.iter().any(|p| p.contains("files_content")), "{plan:?}");
}

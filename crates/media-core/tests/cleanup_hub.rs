//! Cleanup hub foundations: protection on every delete, backups on other drives are kept, variant
//! kinds of similar photos.

use std::fs;
use std::path::{Path, PathBuf};

use media_core::dedupe::{self, DupKind, DupRules, Similarity};
use media_core::scanner::{self, ScanOptions};
use media_core::{journal, protect, volumes, Db, Progress, StopFlag};

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
    let rep = journal::delete_files(&l.db, &[fav, shot, kept], false, &stop, &quiet).unwrap();
    assert_eq!((rep.deleted, rep.protected), (1, 2), "{rep:?}");
    assert!(l.a.join("Pictures/fav.jpg").is_file());
    assert!(!l.a.join("Screenshots/Screenshot_20230101.png").exists());
    let rep = journal::delete_files(&l.db, &[fav], true, &stop, &quiet).unwrap();
    assert_eq!((rep.deleted, rep.protected), (1, 0), "{rep:?}");
    assert!(!l.a.join("Pictures/fav.jpg").exists());
}

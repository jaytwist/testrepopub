//! The folder preview of the library plan: what it shows is what stays. A number on a photo
//! counts the byte-identical files elsewhere that go because this one — the original — stays;
//! those files are never in the preview. A look-alike (same scene, other bytes) stays as well
//! and is shown as such, for the year's review to decide.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use media_core::dedupe::{self, Similarity};
use media_core::library::LibraryOptions;
use media_core::organizer::{self, Action, OrganizeOptions};
use media_core::rusqlite::params;
use media_core::scanner::{self, ScanOptions};
use media_core::{identity, volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

#[test]
fn what_the_preview_shows_stays_and_its_copies_go() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let date = Some(("2017:02:10 10:00:00", "Samsung", "SM-G930"));
    // The phone's photo, the same file twice more under other names (a recovery program's), and
    // a smaller version of it (the same scene, other bytes).
    write_jpeg(&e.join("Phone_SD_Recovery2/DCIM/Camera/20170210_100000.jpg"), &picture(1, 1600, 1200), date);
    for d in ["Phone_SD_Recovery2/Raw2/Graphics/jpeg", "Phone_SD_Recovery2/run_1/Ref_2016"] {
        fs::create_dir_all(e.join(d)).unwrap();
    }
    fs::copy(e.join("Phone_SD_Recovery2/DCIM/Camera/20170210_100000.jpg"), e.join("Phone_SD_Recovery2/Raw2/Graphics/jpeg/f000123.jpg")).unwrap();
    fs::copy(e.join("Phone_SD_Recovery2/DCIM/Camera/20170210_100000.jpg"), e.join("Phone_SD_Recovery2/run_1/Ref_2016/f000777.jpg")).unwrap();
    write_jpeg(&e.join("Phone_SD_Recovery2/Raw2/Graphics/jpeg/f000124.jpg"), &picture(1, 800, 600), date);
    // Another photo of the day, alone.
    write_jpeg(&e.join("Phone_SD_Recovery2/DCIM/Camera/20170210_110000.jpg"), &picture(2, 1600, 1200), Some(("2017:02:10 11:00:00", "Samsung", "SM-G930")));

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    identity::backfill_prehashes(&conn).unwrap();
    let id = |rel: &str| -> i64 { conn.query_row("SELECT id FROM files WHERE rel_path = ?", [rel], |r| r.get(0)).unwrap() };
    // The look of the photos (as analysis stores it): the four of the scene alike, the other not.
    for (r, h) in [
        ("Phone_SD_Recovery2/DCIM/Camera/20170210_100000.jpg", 0i64),
        ("Phone_SD_Recovery2/Raw2/Graphics/jpeg/f000123.jpg", 0),
        ("Phone_SD_Recovery2/run_1/Ref_2016/f000777.jpg", 0),
        ("Phone_SD_Recovery2/Raw2/Graphics/jpeg/f000124.jpg", 0),
        ("Phone_SD_Recovery2/DCIM/Camera/20170210_110000.jpg", i64::MAX),
    ] {
        conn.execute("INSERT OR REPLACE INTO analysis(file_id, version, dhash, analyzed_at) VALUES (?, 1, ?, 0)", params![id(r), h]).unwrap();
    }
    dedupe::find_similar_images(&db, Similarity::High, &stop, &quiet).unwrap();

    let opts = OrganizeOptions { target_dir: e.join("사진").to_string_lossy().into_owned(), layout: "library".into(), library: LibraryOptions::default(), ..Default::default() };
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();

    // Every folder of the plan: what is previewed stays; its number is the copies that go
    // because of it, and none of them is previewed.
    let groups: Vec<String> = plan.items.iter().map(|i| i.group.clone()).collect::<HashSet<_>>().into_iter().filter(|g| !g.is_empty()).collect();
    let mut shown = organizer::group_items(&plan, &groups, 0, 1000);
    organizer::describe(&conn, &mut shown).unwrap();
    let shown_ids: HashSet<i64> = shown.iter().map(|k| k.file_id).collect();
    for k in &shown {
        assert!(matches!(k.action, Action::Move | Action::Copy | Action::InPlace | Action::SkipDuplicate), "{k:?}");
        let gone: Vec<&organizer::PlanItem> = plan.items.iter().filter(|i| i.action == Action::Remove && i.kept == Some(k.file_id)).collect();
        assert_eq!(k.copies as usize, gone.len(), "{k:?}");
        assert_eq!(k.copies_at.len(), gone.len().min(5));
        for g in gone {
            assert!(!shown_ids.contains(&g.file_id), "a copy that goes is in the preview: {}", g.src);
            assert!(k.copies_at.contains(&g.src));
        }
    }

    // The scene: the original with its two identical copies (which go), and the smaller version
    // (other bytes: it stays, marked as a look-alike of the same group).
    let orig = shown.iter().find(|k| k.src.ends_with("20170210_100000.jpg")).expect("the original is in the preview");
    assert_eq!(orig.copies, 2);
    assert!(orig.copies_at.iter().any(|p| p.ends_with("f000123.jpg")) && orig.copies_at.iter().any(|p| p.ends_with("f000777.jpg")));
    assert_eq!((orig.width, orig.height), (Some(1600), Some(1200)));
    // The one kept is the camera's file (its own name, its own folder) — not a recovery
    // program's `f000777.jpg` — and a recovery program's folders are no event: it is filed by date.
    let rel = |p: &str| p.replace('\\', "/").split("/사진/").nth(1).unwrap_or("").to_string();
    let dst = plan.items.iter().find(|i| i.file_id == orig.file_id).unwrap().dst.clone();
    assert_eq!(rel(&dst), "2017/2017-02/20170210_100000.jpg");
    let small_dst = plan.items.iter().find(|i| i.src.ends_with("f000124.jpg")).unwrap().dst.clone();
    assert_eq!(rel(&small_dst), "2017/2017-02/f000124.jpg");
    let small = shown.iter().find(|k| k.src.ends_with("f000124.jpg")).expect("the smaller version stays");
    assert_eq!(small.copies, 0);
    assert!(orig.alike.is_some() && orig.alike == small.alike, "{orig:?} / {small:?}");
    assert_eq!(small.group, orig.group, "it goes beside its original");
    let other = shown.iter().find(|k| k.src.ends_with("20170210_110000.jpg")).unwrap();
    assert_eq!((other.copies, other.alike), (0, None));
    assert!(!shown.iter().any(|k| k.src.ends_with("f000123.jpg") || k.src.ends_with("f000777.jpg")));
    let _ = Path::new("");
}

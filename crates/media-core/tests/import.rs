//! Importing a camera card into the backup drive: only new photos are copied (recognised by
//! content, even renamed), they are indexed as they are written, and the card is forgotten.

use std::fs;

use media_core::importer::{self, ImportSettings};
use media_core::scanner::{self, ScanOptions};
use media_core::{volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

#[test]
fn import_only_new_photos_from_a_card() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let card = drive(tmp.path(), "CARD");
    for (i, name) in ["IMG_0001.JPG", "IMG_0002.JPG", "IMG_0003.JPG"].iter().enumerate() {
        write_jpeg(
            &card.join("DCIM/100CANON").join(name),
            &picture(10 + i as u32, 800, 600),
            Some((&format!("2026:11:0{} 10:00:00", i + 3), "Canon", "EOS R6")),
        );
    }
    // Dumped before, under another name.
    fs::create_dir_all(e.join("사진/old")).unwrap();
    fs::copy(card.join("DCIM/100CANON/IMG_0001.JPG"), e.join("사진/old/renamed.jpg")).unwrap();

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    let opts = ScanOptions::default();
    scanner::scan_all(&db, &opts, &stop, &quiet).unwrap();
    let indexed = |conn: &media_core::rusqlite::Connection| -> i64 { conn.query_row("SELECT COUNT(*) FROM files WHERE missing = 0", [], |r| r.get(0)).unwrap() };
    assert_eq!(indexed(&conn), 1);

    let settings = ImportSettings { target_dir: e.join("사진").to_string_lossy().into_owned(), ..Default::default() };
    let pv = importer::preview(&db, &card, &opts, &stop, &quiet).unwrap();
    assert_eq!((pv.new_files, pv.already), (2, 1), "{pv:?}");

    let rep = importer::run(&db, &card, &settings, &opts, &stop, &quiet).unwrap();
    assert_eq!((rep.copied, rep.already, rep.failed), (2, 1, 0), "{rep:?}");
    assert!(e.join("사진/2026/2026-11/IMG_0002.JPG").is_file());
    assert!(e.join("사진/2026/2026-11/IMG_0003.JPG").is_file());
    assert!(!e.join("사진/2026/2026-11/IMG_0001.JPG").exists(), "already backed up");
    // Indexed while copying (no scan of E), and the card is forgotten; its files stay.
    assert_eq!(indexed(&conn), 3);
    let card_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM files f JOIN volumes v ON v.id = f.volume_id WHERE v.mount LIKE '%CARD%'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(card_rows, 0);
    assert!(card.join("DCIM/100CANON/IMG_0002.JPG").is_file());
    let what: String = conn.query_row("SELECT description FROM batches WHERE id = ?", [&rep.batch_id], |r| r.get(0)).unwrap();
    assert_eq!(what, "SD 카드 ‘CARD’에서 2개 가져오기");
    let vols: i64 = conn.query_row("SELECT COUNT(*) FROM volumes WHERE mount LIKE '%CARD%'", [], |r| r.get(0)).unwrap();
    assert_eq!(vols, 0, "the card leaves no drive entry");

    // The same card again: nothing new.
    let rep = importer::run(&db, &card, &settings, &opts, &stop, &quiet).unwrap();
    assert_eq!((rep.copied, rep.already), (0, 3), "{rep:?}");
    assert_eq!(indexed(&conn), 3);
}

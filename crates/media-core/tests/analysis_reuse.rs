//! Analysis does not redo work: identical copies and RAWs beside their JPG take over results.

use media_core::analysis::{self, ANALYSIS_VERSION, FACE_VERSION};
use media_core::rusqlite::params;
use media_core::Db;

#[test]
fn copies_and_raw_pairs_reuse_results() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(tmp.path().join("library.db")).unwrap();
    let mut conn = db.conn().unwrap();
    conn.execute("INSERT INTO volumes(uid, label, mount, online) VALUES ('v', 'v', '/', 1)", []).unwrap();
    let file = |rel: &str, kind: &str, ext: &str, size: i64, ph: Option<&str>| -> i64 {
        conn.execute(
            "INSERT INTO files(volume_id, rel_path, name, ext, kind, size, mtime, prehash) VALUES (1, ?1, ?1, ?2, ?3, ?4, 0, ?5)",
            params![rel, ext, kind, size, ph],
        )
        .unwrap();
        conn.last_insert_rowid()
    };
    let done = file("trip/IMG_1.JPG", "photo", "jpg", 5000, Some("aaa"));
    let copy = file("backup/IMG_1 (2).JPG", "photo", "jpg", 5000, Some("aaa"));
    let raw = file("trip/IMG_1.CR2", "raw", "cr2", 25000, Some("bbb"));
    let other = file("trip/IMG_2.JPG", "photo", "jpg", 6000, Some("ccc"));
    conn.execute(
        "INSERT INTO analysis(file_id, version, sharpness, dhash, face_count, face_version, analyzed_at) VALUES (?, ?, 12.5, 42, 1, ?, 0)",
        params![done, ANALYSIS_VERSION, FACE_VERSION],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO faces(file_id, x, y, w, h, score, model, embedding) VALUES (?, 0.1, 0.1, 0.2, 0.2, 0.9, 'm', x'00')",
        [done],
    )
    .unwrap();

    let n = analysis::reuse_finished(&mut conn, true).unwrap();
    assert_eq!(n, 2, "the copy and the RAW; the unrelated photo still needs analysis");
    let dhash = |id: i64| -> Option<i64> { conn.query_row("SELECT dhash FROM analysis WHERE file_id = ?", [id], |r| r.get(0)).ok() };
    assert_eq!((dhash(copy), dhash(raw), dhash(other)), (Some(42), Some(42), None));
    let faces = |id: i64| -> i64 { conn.query_row("SELECT COUNT(*) FROM faces WHERE file_id = ?", [id], |r| r.get(0)).unwrap() };
    assert_eq!(faces(copy), 1, "an identical copy has the same faces");
    assert_eq!(faces(raw), 0, "the RAW's faces stay with its JPG (one photo in People)");
    let thumb_from: Option<i64> = conn.query_row("SELECT clone_of FROM files WHERE id = ?", [raw], |r| r.get(0)).unwrap();
    assert_eq!(thumb_from, Some(done), "the RAW shows the JPG's thumbnail instead of decoding the RAW");
    assert_eq!(analysis::pending(&conn, true).unwrap().online, 1);
}

//! People with the real models: scan → analyse (one read: sharpness + faces) → group → name →
//! filter / search. Runs only when the AI files and test photos are available:
//!   MO_AI_DIR       folder with the ONNX Runtime library and the YuNet/SFace models
//!   MO_TEST_FACES   folder with obama*.jpg / biden.jpg (public photos of public officials)
//! Without them (CI) the test passes without doing anything.

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use media_core::analysis::{self, FaceRuntime, PauseFlag};
use media_core::db::{self, FileFilter};
use media_core::faces::FaceEngine;
use media_core::scanner::{self, ScanOptions};
use media_core::{people, thumbs, volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

#[test]
fn groups_real_faces_into_people() {
    let (Some(ai), Some(faces)) = (std::env::var_os("MO_AI_DIR").map(PathBuf::from), std::env::var_os("MO_TEST_FACES").map(PathBuf::from)) else {
        eprintln!("skipped: set MO_AI_DIR and MO_TEST_FACES to run with the real models");
        return;
    };
    let lib = ai.join(if cfg!(windows) { "onnxruntime.dll" } else { "libonnxruntime.so" });
    let rt = FaceRuntime { lib: lib.clone(), models: ai.clone() };
    assert!(rt.available(), "AI files missing in {}", ai.display());

    let tmp = tempfile::tempdir().unwrap();
    let a = drive(tmp.path(), "diskA");
    let photos = a.join("Photos");
    fs::create_dir_all(&photos).unwrap();
    for n in ["obama.jpg", "obama2.jpg", "obama-720p.jpg", "obama-1080p.jpg", "obama-480p.jpg", "biden.jpg", "two_people.jpg"] {
        fs::copy(faces.join(n), photos.join(n)).unwrap();
    }
    // Two more (re-encoded, so different files) photos of Biden: 3 photos make a person.
    let biden = image::open(faces.join("biden.jpg")).unwrap();
    biden.fliph().save(photos.join("biden_flipped.jpg")).unwrap();
    let (w, h) = (biden.width(), biden.height());
    biden.crop_imm(w / 20, h / 20, w * 9 / 10, h * 9 / 10).save(photos.join("biden_crop.jpg")).unwrap();

    let db = Db::open(tmp.path().join("appdata/library.db")).unwrap();
    let mut conn = db.conn().unwrap();
    volumes::add_source(&conn, &a).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    let engine = Mutex::new(FaceEngine::load(&rt.lib, &rt.models, false, 2).unwrap());
    let tc = thumbs::ThumbCache::new(tmp.path().join("appdata/cache"));
    let st = analysis::run(&db, &tc, Some(&engine), 2, &stop, &PauseFlag::default(), &quiet).unwrap();
    assert!(st.faces >= 9, "{st:?}");
    assert_eq!(analysis::pending(&conn, true).unwrap().online, 0, "faces were found in the same single read");

    people::refresh(&mut conn).unwrap();
    let list = people::list(&conn, false).unwrap();
    assert_eq!(list.len(), 2, "Obama and Biden: {list:?}");
    let obama = person_in(&conn, "obama.jpg")[0];
    let biden = person_in(&conn, "biden.jpg")[0];
    assert_ne!(obama, biden);
    for f in ["obama2.jpg", "obama-720p.jpg", "obama-1080p.jpg"] {
        assert_eq!(person_in(&conn, f), vec![obama], "{f}");
    }
    let both = person_in(&conn, "two_people.jpg");
    assert!(both.contains(&obama) && both.contains(&biden), "{both:?}");

    // Name → filter and search.
    people::rename(&mut conn, obama, "Barack").unwrap();
    people::rename(&mut conn, biden, "Joe").unwrap();
    let together = db::query_ids(&conn, &FileFilter { person_ids: Some(vec![obama, biden]), ..Default::default() }).unwrap();
    assert_eq!(together, vec![id_of(&conn, "two_people.jpg")]);
    let found = db::query_ids(&conn, &FileFilter { text: Some("barack".into()), ..Default::default() }).unwrap();
    assert!(found.len() >= 5, "{found:?}");
    let _ = lib;
}

fn id_of(conn: &rusqlite::Connection, name: &str) -> i64 {
    conn.query_row("SELECT id FROM files WHERE name = ?", [name], |r| r.get(0)).unwrap()
}

fn person_in(conn: &rusqlite::Connection, file: &str) -> Vec<i64> {
    let mut st = conn.prepare("SELECT DISTINCT person_id FROM faces WHERE file_id = ? AND person_id IS NOT NULL").unwrap();
    st.query_map([id_of(conn, file)], |r| r.get(0)).unwrap().collect::<Result<Vec<_>, _>>().unwrap()
}

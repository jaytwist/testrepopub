//! People grouping with context: faces in one photo are different people, faces from the same
//! folder within a few days ("same event") group a little more readily; sending faces to another
//! person sticks; "same person?" suggestions skip people seen together and show shared events.

use media_core::analysis::embedding_to_blob;
use media_core::faces::l2_normalize;
use media_core::rusqlite::{params, Connection};
use media_core::{people, Db};

const DAY: i64 = 86_400;

/// cos·e0 + sin·e_k: two such faces (different k) have similarity cos².
fn face_vec(cos2: f32, k: usize) -> Vec<f32> {
    let c = cos2.sqrt();
    let mut v = vec![0f32; 128];
    v[0] = c;
    if k > 0 {
        v[k] = (1.0 - c * c).sqrt();
    }
    l2_normalize(&v)
}

struct Lib {
    _tmp: tempfile::TempDir,
    _db: Db,
    conn: Connection,
}

fn lib() -> Lib {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(tmp.path().join("library.db")).unwrap();
    let conn = db.conn().unwrap();
    conn.execute("INSERT INTO volumes(uid, label, mount, online) VALUES ('v', 'v', '/', 1)", []).unwrap();
    Lib { _tmp: tmp, _db: db, conn }
}

impl Lib {
    fn file(&self, rel: &str, taken: i64) -> i64 {
        self.file_dated(rel, taken, "exif")
    }
    fn file_dated(&self, rel: &str, taken: i64, source: &str) -> i64 {
        self.conn
            .execute(
                "INSERT INTO files(volume_id, rel_path, name, ext, kind, size, mtime, taken_at, date_source, width, height)
                 VALUES (1, ?1, ?1, 'jpg', 'photo', 1, 0, ?2, ?3, 4000, 3000)",
                params![rel, taken, source],
            )
            .unwrap();
        self.conn.last_insert_rowid()
    }
    fn face(&self, file: i64, v: &[f32], person: Option<i64>) -> i64 {
        self.conn
            .execute(
                "INSERT INTO faces(file_id, x, y, w, h, score, model, embedding, person_id) VALUES (?1, 0.4, 0.4, 0.1, 0.1, 0.95, 'sface-2021dec', ?2, ?3)",
                params![file, embedding_to_blob(v), person],
            )
            .unwrap();
        self.conn.last_insert_rowid()
    }
    fn person(&self, name: &str) -> i64 {
        self.conn.execute("INSERT INTO persons(name, created_at) VALUES (?, 0)", [name]).unwrap();
        self.conn.last_insert_rowid()
    }
    fn persons(&self) -> i64 {
        self.conn.query_row("SELECT COUNT(*) FROM persons", [], |r| r.get(0)).unwrap()
    }
    fn person_of(&self, face: i64) -> Option<i64> {
        self.conn.query_row("SELECT person_id FROM faces WHERE id = ?", [face], |r| r.get(0)).unwrap()
    }
}

/// Three faces whose pairwise similarity (0.39) is just under the grouping threshold.
fn three_borderline(l: &Lib, places: [(&str, i64); 3]) {
    for (k, (rel, day)) in places.into_iter().enumerate() {
        let f = l.file(rel, 1_700_000_000 + day * DAY);
        l.face(f, &face_vec(0.39, k + 1), None);
    }
}

#[test]
fn same_event_groups_borderline_faces() {
    let l = lib();
    three_borderline(&l, [("trip/a.jpg", 0), ("trip/b.jpg", 1), ("trip/c.jpg", 2)]);
    let mut conn = l.conn;
    people::refresh(&mut conn).unwrap();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM persons", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "same folder, same few days: one person");
}

#[test]
fn different_folders_or_years_do_not() {
    let mut l = lib();
    three_borderline(&l, [("x/a.jpg", 0), ("y/b.jpg", 0), ("z/c.jpg", 0)]);
    people::refresh(&mut l.conn).unwrap();
    assert_eq!(l.persons(), 0, "different folders: looks alone decide");

    let mut l = lib();
    three_borderline(&l, [("Camera Roll/a.jpg", 0), ("Camera Roll/b.jpg", 400), ("Camera Roll/c.jpg", 800)]);
    people::refresh(&mut l.conn).unwrap();
    assert_eq!(l.persons(), 0, "same folder years apart is not an event");

    // Copied / downloaded photos: the only date is the copy time, shared by everyone in the folder.
    let mut l = lib();
    for k in 1..=3 {
        let f = l.file_dated(&format!("카카오톡 받은 파일/{k}.jpg"), 1_700_000_000, "mtime");
        l.face(f, &face_vec(0.39, k), None);
    }
    people::refresh(&mut l.conn).unwrap();
    assert_eq!(l.persons(), 0, "a file-time date is not an event");
}

#[test]
fn one_photo_never_holds_the_same_person_twice() {
    let mut l = lib();
    let v = face_vec(1.0, 0);
    let group = l.file("p/group.jpg", 0);
    let a = l.face(group, &v, None);
    let b = l.face(group, &v, None); // identical looks, same photo (e.g. twins, a reflection)
    let c = l.face(l.file("p/c.jpg", 0), &v, None);
    let d = l.face(l.file("p/d.jpg", 0), &v, None);
    people::refresh(&mut l.conn).unwrap();
    let p = l.person_of(a).expect("a, c, d form a person");
    assert_eq!((l.person_of(c), l.person_of(d)), (Some(p), Some(p)));
    assert_ne!(l.person_of(b), Some(p));

    // Joining an existing person respects it too.
    let e = l.face(group, &v, None);
    people::refresh(&mut l.conn).unwrap();
    assert_ne!(l.person_of(e), Some(p));
}

#[test]
fn sending_faces_to_another_person_sticks() {
    let mut l = lib();
    let mom = l.person("엄마");
    let dad = l.person("아빠");
    let vm = face_vec(1.0, 0);
    let vd = face_vec(0.0, 5);
    let m: Vec<i64> = (0..3).map(|i| l.face(l.file(&format!("m{i}.jpg"), 0), &vm, Some(mom))).collect();
    let wrong = l.face(l.file("w.jpg", 0), &vm, Some(mom)); // looks like mom, but is dad
    (0..3).for_each(|i| {
        l.face(l.file(&format!("d{i}.jpg"), 0), &vd, Some(dad));
    });

    let dad_cover = people::list(&l.conn, false).unwrap().into_iter().find(|p| p.id == dad).unwrap().cover_face_id;
    assert_eq!(people::move_faces(&mut l.conn, &[wrong], mom, Some(dad)).unwrap(), dad);
    let after = people::list(&l.conn, false).unwrap().into_iter().find(|p| p.id == dad).unwrap().cover_face_id;
    assert_eq!(after, dad_cover, "the target's picture does not jump to the moved face");
    assert_eq!(l.person_of(wrong), Some(dad));
    // Grouping again does not pull it back to mom.
    l.conn.execute("UPDATE faces SET locked = 0", []).unwrap();
    l.conn.execute("UPDATE faces SET person_id = NULL WHERE id = ?", [wrong]).unwrap();
    people::refresh(&mut l.conn).unwrap();
    assert_ne!(l.person_of(wrong), Some(mom));

    // To a new person; an unnamed person left empty disappears.
    let fresh = people::move_faces(&mut l.conn, &m[..1], mom, None).unwrap();
    assert!(fresh != mom && fresh != dad);
    assert_eq!(l.person_of(m[0]), Some(fresh));
    // Shown in the list although it has a single photo: the user made it.
    assert!(people::list(&l.conn, false).unwrap().iter().any(|p| p.id == fresh));
    let back = people::move_faces(&mut l.conn, &m[..1], fresh, Some(mom)).unwrap();
    assert_eq!(back, mom);
    let gone: i64 = l.conn.query_row("SELECT COUNT(*) FROM persons WHERE id = ?", [fresh], |r| r.get(0)).unwrap();
    assert_eq!(gone, 0);
    assert!(people::move_faces(&mut l.conn, &m[..1], mom, Some(mom)).is_err());
}

#[test]
fn suggestions_skip_people_seen_together_and_show_shared_events() {
    // Two cards that look alike (0.6), photographed at the same event but never together.
    let l = lib();
    let (a, b) = (l.person(""), l.person(""));
    l.conn.execute("UPDATE persons SET name = NULL", []).unwrap();
    for i in 0..3 {
        l.face(l.file(&format!("party/a{i}.jpg"), 1_700_000_000), &face_vec(1.0, 0), Some(a));
        l.face(l.file(&format!("party/b{i}.jpg"), 1_700_000_000 + DAY), &face_vec(0.6, 1), Some(b));
    }
    let s = people::suggestions(&l.conn, 10).unwrap();
    assert_eq!(s.len(), 1);
    assert!(s[0].shared_events >= 1, "{}", s[0].shared_events);

    // Once they appear in one photo together, they are two people: no suggestion.
    let together = l.file("party/both.jpg", 1_700_000_000);
    l.face(together, &face_vec(1.0, 0), Some(a));
    l.face(together, &face_vec(0.6, 1), Some(b));
    assert!(people::suggestions(&l.conn, 10).unwrap().is_empty());
}

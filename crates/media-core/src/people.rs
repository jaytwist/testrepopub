//! People: group detected faces (SFace embeddings) into persons, incrementally.
//!
//! Proven recipe (Immich's incremental clustering, digiKam's "learn only from confirmed faces"):
//!   1. a new face joins the most similar existing person (similarity to that person's reference),
//!      unless the user said "not this person" for that face;
//!   2. faces left over are grouped among themselves; a group of `MIN_FACES`+ becomes a new
//!      person with *all* its members (Immich's known bug: only the last face went in);
//!   3. smaller groups wait (deferred) and are tried again after the next analysis pass.
//! Faces the user confirmed (named / merged) are never moved, and a person's reference is built
//! from confirmed faces when it has any.

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::analysis::blob_to_embedding;
use crate::faces::{l2_normalize, similarity, EMBED_DIM};

/// Join an existing person at or above this similarity (SFace, measured in the spike: same
/// person 0.73–0.98, different people 0.07–0.27; OpenCV's own threshold is 0.363).
const ASSIGN_SIM: f32 = 0.40;
/// Group unassigned faces at or above this similarity.
const LINK_SIM: f32 = 0.42;
/// A group needs this many faces to become a person (Immich's default, too).
pub const MIN_FACES: usize = 3;
/// "Same person?" suggestions: references this similar but not merged.
const SUGGEST_SIM: f32 = 0.33;
/// Faces too small or too uncertain to group reliably (still shown on their photo).
const MIN_SCORE: f32 = 0.8;
const MIN_SIDE_PX: f64 = 40.0;
/// Faces are detected on an image of at most this many pixels per side (see `analysis`).
const ANALYSIS_SIDE: f64 = 1280.0;

struct FaceVec {
    id: i64,
    person: Option<i64>,
    locked: bool,
    v: Vec<f32>,
}

fn mean(vs: &[&[f32]]) -> Vec<f32> {
    let mut m = vec![0f32; EMBED_DIM];
    for v in vs {
        for (a, b) in m.iter_mut().zip(v.iter()) {
            *a += b;
        }
    }
    l2_normalize(&m)
}

/// Faces good enough to group (live files, not hidden).
fn load_faces(conn: &Connection) -> Result<Vec<FaceVec>> {
    let mut st = conn.prepare(
        "SELECT fc.id, fc.person_id, fc.locked, fc.embedding, fc.score, fc.w, fc.h, f.width, f.height
         FROM faces fc JOIN files f ON f.id = fc.file_id
         WHERE fc.hidden = 0 AND f.missing = 0 AND f.quarantined = 0",
    )?;
    let rows = st
        .query_map([], |r| {
            let score: f64 = r.get(4)?;
            let (w, h): (f64, f64) = (r.get(5)?, r.get(6)?);
            let (fw, fh): (Option<i64>, Option<i64>) = (r.get(7)?, r.get(8)?);
            let side_ok = match (fw, fh) {
                (Some(fw), Some(fh)) if fw > 0 && fh > 0 => {
                    let scale = (ANALYSIS_SIDE / fw.max(fh) as f64).min(1.0);
                    (w * fw as f64).min(h * fh as f64) * scale >= MIN_SIDE_PX
                }
                _ => true,
            };
            let blob: Vec<u8> = r.get(3)?;
            Ok((score >= MIN_SCORE as f64 && side_ok, FaceVec { id: r.get(0)?, person: r.get(1)?, locked: r.get::<_, i64>(2)? != 0, v: blob_to_embedding(&blob) }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().filter(|(ok, f)| (*ok || f.person.is_some()) && f.v.len() == EMBED_DIM).map(|(_, f)| f).collect())
}

/// Each person's reference: mean of confirmed faces if any, else of all its faces.
fn references(faces: &[FaceVec], hidden_ok: &HashSet<i64>) -> HashMap<i64, Vec<f32>> {
    let mut locked: HashMap<i64, Vec<&[f32]>> = HashMap::new();
    let mut all: HashMap<i64, Vec<&[f32]>> = HashMap::new();
    for f in faces {
        if let Some(p) = f.person {
            if !hidden_ok.contains(&p) {
                continue;
            }
            all.entry(p).or_default().push(&f.v);
            if f.locked {
                locked.entry(p).or_default().push(&f.v);
            }
        }
    }
    all.into_iter().map(|(p, vs)| (p, mean(locked.get(&p).map(|l| l.as_slice()).unwrap_or(&vs)))).collect()
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PeopleStats {
    pub assigned: usize,
    pub new_people: usize,
    pub deferred: usize,
}

/// Group new faces. Cheap when little changed; runs after every analysis pass.
pub fn refresh(conn: &mut Connection) -> Result<PeopleStats> {
    let faces = load_faces(conn)?;
    // Hidden people still collect their faces (so strangers stay out of the way).
    let persons: HashSet<i64> = {
        let mut st = conn.prepare("SELECT id FROM persons")?;
        let rows = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<HashSet<i64>>>()?;
        rows
    };
    let rejects: HashSet<(i64, i64)> = {
        let mut st = conn.prepare("SELECT face_id, person_id FROM face_rejects")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<HashSet<_>>>()?;
        rows
    };
    let refs = references(&faces, &persons);
    let ref_list: Vec<(i64, &Vec<f32>)> = refs.iter().map(|(p, v)| (*p, v)).collect();
    let unassigned: Vec<&FaceVec> = faces.iter().filter(|f| f.person.is_none()).collect();
    let mut stats = PeopleStats::default();

    // 1. Join existing people.
    let joins: Vec<(i64, Option<i64>)> = unassigned
        .par_iter()
        .map(|f| {
            let best = ref_list
                .iter()
                .filter(|(p, _)| !rejects.contains(&(f.id, *p)))
                .map(|(p, r)| (*p, similarity(&f.v, r)))
                .filter(|(_, s)| *s >= ASSIGN_SIM)
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(p, _)| p);
            (f.id, best)
        })
        .collect();
    let joined: HashMap<i64, i64> = joins.into_iter().filter_map(|(f, p)| p.map(|p| (f, p))).collect();
    stats.assigned = joined.len();

    // 2. Group the rest (leader-follower on running group references).
    let rest: Vec<&FaceVec> = unassigned.into_iter().filter(|f| !joined.contains_key(&f.id)).collect();
    struct Group {
        sum: Vec<f32>,
        reference: Vec<f32>,
        members: Vec<i64>,
    }
    let mut groups: Vec<Group> = Vec::new();
    for f in &rest {
        let best = groups
            .par_iter()
            .enumerate()
            .map(|(i, g)| (i, similarity(&f.v, &g.reference)))
            .filter(|(_, s)| *s >= LINK_SIM)
            .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)));
        match best {
            Some((i, _)) => {
                let g = &mut groups[i];
                for (a, b) in g.sum.iter_mut().zip(f.v.iter()) {
                    *a += b;
                }
                g.reference = l2_normalize(&g.sum);
                g.members.push(f.id);
            }
            None => groups.push(Group { sum: f.v.clone(), reference: f.v.clone(), members: vec![f.id] }),
        }
    }

    let tx = conn.transaction()?;
    {
        let mut set = tx.prepare("UPDATE faces SET person_id = ? WHERE id = ? AND person_id IS NULL")?;
        for (f, p) in &joined {
            set.execute(params![p, f])?;
        }
        let now = crate::now_ts();
        for g in groups.iter().filter(|g| g.members.len() >= MIN_FACES) {
            tx.execute("INSERT INTO persons(name, cover_face_id, hidden, created_at) VALUES (NULL, NULL, 0, ?)", [now])?;
            let pid = tx.last_insert_rowid();
            for f in &g.members {
                set.execute(params![pid, f])?;
            }
            stats.new_people += 1;
        }
        stats.deferred = groups.iter().filter(|g| g.members.len() < MIN_FACES).map(|g| g.members.len()).sum();
        // People left with no faces (all removed / files deleted) disappear.
        tx.execute("DELETE FROM persons WHERE name IS NULL AND NOT EXISTS (SELECT 1 FROM faces WHERE person_id = persons.id)", [])?;
    }
    tx.commit()?;
    Ok(stats)
}

// ---------------------------------------------------------------------------------------------
// User actions
// ---------------------------------------------------------------------------------------------

/// Another person already called `name` (case-insensitive), if any.
pub fn person_named(conn: &Connection, name: &str, except: i64) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT id FROM persons WHERE name = ? COLLATE NOCASE AND id != ? LIMIT 1", params![name.trim(), except], |r| r.get(0))
        .optional()?)
}

/// Name a person (or clear the name with ""). Naming confirms its faces. Fails if another person
/// already has the name — the UI then offers to merge (`merge`).
pub fn rename(conn: &mut Connection, person: i64, name: &str) -> Result<()> {
    let name = name.trim();
    if !name.is_empty() && person_named(conn, name, person)?.is_some() {
        bail!("같은 이름의 인물이 이미 있습니다");
    }
    let tx = conn.transaction()?;
    tx.execute("UPDATE persons SET name = NULLIF(?, '') WHERE id = ?", params![name, person])?;
    if !name.is_empty() {
        tx.execute("UPDATE faces SET locked = 1 WHERE person_id = ?", [person])?;
    }
    tx.commit()?;
    Ok(())
}

/// Merge `others` into `into` (faces confirmed). Keeps `into`'s name, or takes the first name found.
pub fn merge(conn: &mut Connection, into: i64, others: &[i64]) -> Result<()> {
    let tx = conn.transaction()?;
    for o in others.iter().filter(|o| **o != into) {
        tx.execute(
            "UPDATE persons SET name = COALESCE(name, (SELECT name FROM persons WHERE id = ?2)) WHERE id = ?1",
            params![into, o],
        )?;
        tx.execute("UPDATE faces SET person_id = ?, locked = 1 WHERE person_id = ?", params![into, o])?;
        tx.execute("UPDATE OR IGNORE face_rejects SET person_id = ? WHERE person_id = ?", params![into, o])?;
        tx.execute("DELETE FROM face_rejects WHERE person_id = ?", [o])?;
        tx.execute("DELETE FROM person_pairs_no WHERE a = ?1 OR b = ?1", [o])?;
        tx.execute("DELETE FROM persons WHERE id = ?", [o])?;
    }
    tx.execute("UPDATE faces SET locked = 1 WHERE person_id = ?", [into])?;
    tx.commit()?;
    Ok(())
}

/// "Not this person": the faces leave the person and never join it again.
pub fn reject(conn: &mut Connection, face_ids: &[i64], person: i64) -> Result<()> {
    let tx = conn.transaction()?;
    for f in face_ids {
        tx.execute("INSERT OR IGNORE INTO face_rejects(face_id, person_id) VALUES (?, ?)", params![f, person])?;
        tx.execute("UPDATE faces SET person_id = NULL, locked = 0 WHERE id = ? AND person_id = ?", params![f, person])?;
        tx.execute("UPDATE persons SET cover_face_id = NULL WHERE id = ? AND cover_face_id = ?", params![person, f])?;
    }
    tx.commit()?;
    Ok(())
}

pub fn set_hidden(conn: &Connection, persons: &[i64], hidden: bool) -> Result<()> {
    for p in persons {
        conn.execute("UPDATE persons SET hidden = ? WHERE id = ?", params![hidden as i64, p])?;
    }
    Ok(())
}

pub fn set_cover(conn: &Connection, person: i64, face: i64) -> Result<()> {
    conn.execute("UPDATE persons SET cover_face_id = ? WHERE id = ? AND EXISTS (SELECT 1 FROM faces WHERE id = ? AND person_id = ?)", params![face, person, face, person])?;
    Ok(())
}

/// Answer to "same person?": different pairs are remembered and not asked again.
pub fn answer_pair(conn: &mut Connection, a: i64, b: i64, same: bool) -> Result<()> {
    if same {
        // Keep the named one (or the bigger one) as the target.
        let score = |p: i64| -> Result<(i64, i64)> {
            Ok(conn.query_row(
                "SELECT name IS NOT NULL, (SELECT COUNT(*) FROM faces WHERE person_id = ?1) FROM persons WHERE id = ?1",
                [p],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        };
        let (into, other) = if score(a)? >= score(b)? { (a, b) } else { (b, a) };
        merge(conn, into, &[other])
    } else {
        let (x, y) = (a.min(b), a.max(b));
        conn.execute("INSERT OR IGNORE INTO person_pairs_no(a, b) VALUES (?, ?)", params![x, y])?;
        Ok(())
    }
}

/// Forget every face and person (the "delete face data" button). Photos stay; turning the
/// feature back on analyses faces again.
pub fn clear_all(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute_batch(
        "DELETE FROM face_rejects; DELETE FROM person_pairs_no; DELETE FROM faces; DELETE FROM persons;
         UPDATE analysis SET face_version = 0, face_count = NULL;",
    )?;
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Queries for the UI
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct PersonRow {
    pub id: i64,
    pub name: Option<String>,
    pub cover_face_id: Option<i64>,
    pub photos: i64,
    pub hidden: bool,
}

/// People sorted by photo count (named people first among equals). Unnamed people with fewer
/// than `MIN_FACES` photos (after removals) are left out.
pub fn list(conn: &Connection, include_hidden: bool) -> Result<Vec<PersonRow>> {
    let mut st = conn.prepare(
        "SELECT p.id, p.name, p.cover_face_id, p.hidden, COUNT(DISTINCT fc.file_id),
                (SELECT fc2.id FROM faces fc2 JOIN files f2 ON f2.id = fc2.file_id
                  WHERE fc2.person_id = p.id AND f2.missing = 0 AND f2.quarantined = 0
                  ORDER BY fc2.locked DESC, fc2.score * fc2.w * fc2.h DESC LIMIT 1)
         FROM persons p JOIN faces fc ON fc.person_id = p.id JOIN files f ON f.id = fc.file_id
         WHERE f.missing = 0 AND f.quarantined = 0 AND (?1 OR p.hidden = 0)
         GROUP BY p.id
         HAVING p.name IS NOT NULL OR COUNT(DISTINCT fc.file_id) >= ?2
         ORDER BY COUNT(DISTINCT fc.file_id) DESC, p.name IS NULL, p.id",
    )?;
    let rows = st
        .query_map(params![include_hidden, MIN_FACES as i64], |r| {
            let cover: Option<i64> = r.get(2)?;
            Ok(PersonRow { id: r.get(0)?, name: r.get(1)?, hidden: r.get::<_, i64>(3)? != 0, photos: r.get(4)?, cover_face_id: cover.or(r.get(5)?) })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Clone, Debug, Serialize)]
pub struct FaceRow {
    pub id: i64,
    pub file_id: i64,
    pub locked: bool,
}

/// A person's faces, best first.
pub fn faces_of(conn: &Connection, person: i64, limit: i64) -> Result<Vec<FaceRow>> {
    let mut st = conn.prepare(
        "SELECT fc.id, fc.file_id, fc.locked FROM faces fc JOIN files f ON f.id = fc.file_id
         WHERE fc.person_id = ? AND f.missing = 0 AND f.quarantined = 0
         ORDER BY fc.locked DESC, fc.score * fc.w * fc.h DESC LIMIT ?",
    )?;
    let rows = st
        .query_map(params![person, limit], |r| Ok(FaceRow { id: r.get(0)?, file_id: r.get(1)?, locked: r.get::<_, i64>(2)? != 0 }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Clone, Debug, Serialize)]
pub struct PairSuggestion {
    pub a: PersonRow,
    pub b: PersonRow,
    pub similarity: f32,
}

/// "Same person?" — the most similar pairs of visible people that were not answered "different"
/// and do not carry two different names. At most `limit`.
pub fn suggestions(conn: &Connection, limit: usize) -> Result<Vec<PairSuggestion>> {
    let people: HashMap<i64, PersonRow> = list(conn, false)?.into_iter().map(|p| (p.id, p)).collect();
    let faces = load_faces(conn)?;
    let ids: HashSet<i64> = people.keys().copied().collect();
    let refs = references(&faces, &ids);
    let no: HashSet<(i64, i64)> = {
        let mut st = conn.prepare("SELECT a, b FROM person_pairs_no")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<HashSet<_>>>()?;
        rows
    };
    let list: Vec<(&i64, &Vec<f32>)> = refs.iter().collect();
    let mut out: Vec<(i64, i64, f32)> = Vec::new();
    for (i, (a, ra)) in list.iter().enumerate() {
        for (b, rb) in &list[i + 1..] {
            let (x, y) = ((**a).min(**b), (**a).max(**b));
            if no.contains(&(x, y)) {
                continue;
            }
            if let (Some(na), Some(nb)) = (&people[a].name, &people[b].name) {
                if !na.eq_ignore_ascii_case(nb) {
                    continue;
                }
            }
            let s = similarity(ra, rb);
            if s >= SUGGEST_SIM {
                out.push((x, y, s));
            }
        }
    }
    out.sort_by(|p, q| q.2.total_cmp(&p.2));
    Ok(out
        .into_iter()
        .take(limit)
        .map(|(a, b, s)| PairSuggestion { a: people[&a].clone(), b: people[&b].clone(), similarity: s })
        .collect())
}

/// Named people in a photo (for the viewer).
pub fn names_in(conn: &Connection, file_id: i64) -> Result<Vec<(i64, String)>> {
    let mut st = conn.prepare(
        "SELECT DISTINCT p.id, p.name FROM faces fc JOIN persons p ON p.id = fc.person_id
         WHERE fc.file_id = ? AND p.name IS NOT NULL AND p.hidden = 0 ORDER BY p.name",
    )?;
    let rows = st.query_map([file_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The original face a copied face came from (its crop can be reused).
pub fn src_face(conn: &Connection, face_id: i64) -> Result<Option<i64>> {
    Ok(conn.query_row("SELECT src_face FROM faces WHERE id = ?", [face_id], |r| r.get(0)).optional()?.flatten())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::embedding_to_blob;
    use crate::Db;

    /// A random-ish unit vector per identity, and noisy samples of it (similarity ~0.8).
    fn identity(seed: u32) -> Vec<f32> {
        let mut x = seed.wrapping_mul(2654435761).wrapping_add(12345);
        let v: Vec<f32> = (0..EMBED_DIM)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x % 2000) as f32 / 1000.0 - 1.0
            })
            .collect();
        l2_normalize(&v)
    }
    fn sample(id: &[f32], seed: u32) -> Vec<f32> {
        let noise = identity(seed.wrapping_add(10_000));
        l2_normalize(&id.iter().zip(&noise).map(|(a, n)| a + 0.45 * n).collect::<Vec<_>>())
    }

    struct Lib {
        _tmp: tempfile::TempDir,
        conn: Connection,
        next: i64,
    }
    impl Lib {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let db = Db::open(tmp.path().join("l.db")).unwrap();
            let conn = db.conn().unwrap();
            conn.execute("INSERT INTO volumes(id, uid, label, mount, removable, online, last_seen) VALUES (1, 'v', 'v', '/', 0, 1, 0)", []).unwrap();
            Lib { _tmp: tmp, conn, next: 1 }
        }
        /// A photo with one face of `who`; returns the face id.
        fn photo(&mut self, who: &[f32]) -> i64 {
            let n = self.next;
            self.next += 1;
            self.conn
                .execute(
                    "INSERT INTO files(id, volume_id, rel_path, name, ext, kind, size, mtime, date_source, width, height, missing, quarantined, scanned_at)
                     VALUES (?1, 1, ?2, ?2, 'jpg', 'photo', 1, 0, 'exif', 1000, 1000, 0, 0, 0)",
                    params![n, format!("p{n}.jpg")],
                )
                .unwrap();
            self.conn
                .execute(
                    "INSERT INTO faces(file_id, x, y, w, h, score, sharpness, model, embedding) VALUES (?, 0.1, 0.1, 0.2, 0.2, 0.95, 50, 'sface', ?)",
                    params![n, embedding_to_blob(&sample(who, n as u32))],
                )
                .unwrap();
            self.conn.last_insert_rowid()
        }
        fn person_of(&self, face: i64) -> Option<i64> {
            self.conn.query_row("SELECT person_id FROM faces WHERE id = ?", [face], |r| r.get(0)).unwrap()
        }
    }

    #[test]
    fn samples_are_realistic() {
        let (a, b) = (identity(1), identity(2));
        let same = similarity(&sample(&a, 1), &sample(&a, 2));
        let diff = similarity(&sample(&a, 1), &sample(&b, 3));
        assert!(same > 0.6 && diff < 0.3, "same {same} diff {diff}");
    }

    #[test]
    fn groups_two_people_then_assigns_new_faces() {
        let mut l = Lib::new();
        let (mom, dad) = (identity(1), identity(2));
        let m: Vec<i64> = (0..4).map(|_| l.photo(&mom)).collect();
        let d: Vec<i64> = (0..3).map(|_| l.photo(&dad)).collect();
        let stranger = l.photo(&identity(3));
        let s = refresh(&mut l.conn).unwrap();
        assert_eq!(s.new_people, 2, "{s:?}");
        let pm = l.person_of(m[0]).unwrap();
        // Every member of the new group is in it (not just the last one).
        assert!(m.iter().all(|f| l.person_of(*f) == Some(pm)));
        assert!(d.iter().all(|f| l.person_of(*f) == l.person_of(d[0])));
        assert_ne!(l.person_of(d[0]), Some(pm));
        assert_eq!(l.person_of(stranger), None, "one face is not a person (deferred)");
        // A new photo of mom joins mom.
        let m5 = l.photo(&mom);
        let s = refresh(&mut l.conn).unwrap();
        assert_eq!((s.assigned, s.new_people), (1, 0));
        assert_eq!(l.person_of(m5), Some(pm));
        // The deferred stranger becomes a person once there are 3 photos of them.
        l.photo(&identity(3));
        l.photo(&identity(3));
        assert_eq!(refresh(&mut l.conn).unwrap().new_people, 1);
        assert!(l.person_of(stranger).is_some());
    }

    #[test]
    fn user_decisions_stick() {
        let mut l = Lib::new();
        let mom = identity(1);
        let m: Vec<i64> = (0..4).map(|_| l.photo(&mom)).collect();
        refresh(&mut l.conn).unwrap();
        let pm = l.person_of(m[0]).unwrap();
        rename(&mut l.conn, pm, "Mom").unwrap();
        // "Not this person" — and it does not come back on the next pass.
        reject(&mut l.conn, &[m[3]], pm).unwrap();
        refresh(&mut l.conn).unwrap();
        assert_eq!(l.person_of(m[3]), None);
        assert_eq!(l.person_of(m[0]), Some(pm));
        assert_eq!(list(&l.conn, false).unwrap()[0].name.as_deref(), Some("Mom"));
        assert_eq!(names_in(&l.conn, 1).unwrap(), vec![(pm, "Mom".to_string())]);
    }

    #[test]
    fn same_name_merges_and_answers_are_remembered() {
        let mut l = Lib::new();
        let mom = identity(1);
        // Two separate groups of the same person (as happens with age / angle changes): force
        // them apart by creating one, naming it, then adding faces the reference does not reach.
        let a: Vec<i64> = (0..3).map(|_| l.photo(&mom)).collect();
        refresh(&mut l.conn).unwrap();
        let pa = l.person_of(a[0]).unwrap();
        let other = identity(9);
        let b: Vec<i64> = (0..3).map(|_| l.photo(&other)).collect();
        refresh(&mut l.conn).unwrap();
        let pb = l.person_of(b[0]).unwrap();
        rename(&mut l.conn, pa, "Mom").unwrap();
        assert!(rename(&mut l.conn, pb, "mom").is_err(), "same name → the UI offers to merge");
        assert_eq!(person_named(&l.conn, "mom", pb).unwrap(), Some(pa));
        merge(&mut l.conn, pa, &[pb]).unwrap();
        assert!(b.iter().all(|f| l.person_of(*f) == Some(pa)));
        assert_eq!(list(&l.conn, false).unwrap().len(), 1);

        clear_all(&mut l.conn).unwrap();
        assert!(list(&l.conn, true).unwrap().is_empty());
    }

    #[test]
    fn same_person_question() {
        let mut l = Lib::new();
        // Two look-alike groups: references ~0.37 apart — separate people, but worth asking.
        let x = identity(1);
        let y = l2_normalize(&x.iter().zip(identity(50)).map(|(a, b)| a + 2.2 * b).collect::<Vec<_>>());
        let fx: Vec<i64> = (0..4).map(|_| l.photo(&x)).collect();
        let fy: Vec<i64> = (0..4).map(|_| l.photo(&y)).collect();
        refresh(&mut l.conn).unwrap();
        let (px, py) = (l.person_of(fx[0]).unwrap(), l.person_of(fy[0]).unwrap());
        assert_ne!(px, py);
        let pair = |l: &Lib| suggestions(&l.conn, 10).unwrap().iter().any(|s| (s.a.id.min(s.b.id), s.a.id.max(s.b.id)) == (px.min(py), px.max(py)));
        assert!(pair(&l), "look-alikes are suggested");
        // "Different" is remembered…
        answer_pair(&mut l.conn, px, py, false).unwrap();
        assert!(!pair(&l));
        refresh(&mut l.conn).unwrap();
        assert!(!pair(&l));
        // …"same" merges into one person.
        l.conn.execute("DELETE FROM person_pairs_no", []).unwrap();
        answer_pair(&mut l.conn, px, py, true).unwrap();
        assert_eq!(list(&l.conn, false).unwrap().len(), 1);
        assert_eq!(l.person_of(fx[0]), l.person_of(fy[0]));
    }
}

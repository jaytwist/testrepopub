//! "100% 복사 폴더": folders that were copied whole. Two folders on the same drive whose photos
//! and videos (sub-folders included) are the same in number and content are one folder and its
//! copies; the oldest stays and the copies go.
//!
//! Once a library drive is chosen, only that drive is looked at and touched: the other drives
//! stay exactly as they are (their folders are the backup, not copies to clear).
//!
//! Finding them reads only the index: every folder gets a signature over its whole subtree
//! (count, bytes, sum of content fingerprints), equal signatures are compared file by file, and
//! only the topmost folders are reported (`A` = `B`, not also `A/파리` = `B/파리`). Removing them
//! goes through the journal like every other copy: each file is quarantined with its twin in the
//! folder that stays and deleted only after the two were compared byte for byte.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::dedupe::DupRules;
use crate::{db, journal, library, volumes, Db, Progress, ProgressFn, StopFlag};

const JOB: &str = "cleanup:folders";

// ---------------------------------------------------------------------------------------------
// The index, by drive
// ---------------------------------------------------------------------------------------------

struct Rec {
    id: i64,
    rel: String,
    size: i64,
    /// Content fingerprint; `None` for a row an older version indexed and nothing has read since.
    key: Option<String>,
    video: bool,
    /// Wall-clock time taken, if known.
    taken: Option<i64>,
    /// A photo the analysis could not open: likely damaged.
    bad: bool,
}

struct Vol {
    id: i64,
    mount: String,
    /// Sorted by `rel`: a folder's subtree is one contiguous run.
    files: Vec<Rec>,
}

impl Vol {
    fn subtree(&self, dir: &str) -> &[Rec] {
        let prefix = format!("{dir}/");
        let lo = self.files.partition_point(|r| r.rel.as_str() < prefix.as_str());
        let n = self.files[lo..].partition_point(|r| r.rel.starts_with(&prefix));
        &self.files[lo..lo + n]
    }

    fn abs(&self, rel: &str) -> PathBuf {
        volumes::join_rel(Path::new(&self.mount), rel)
    }
}

/// Live files of the connected drives.
fn load(conn: &Connection) -> Result<Vec<Vol>> {
    let mut vols: Vec<Vol> = Vec::new();
    let mut at: HashMap<i64, usize> = HashMap::new();
    let mut st = conn.prepare(
        "SELECT f.id, f.volume_id, f.rel_path, f.size, COALESCE(f.prehash, f.hash), v.mount, f.kind, f.taken_at,
                (a.file_id IS NOT NULL AND a.dhash IS NULL AND f.kind = 'photo')
         FROM files f JOIN volumes v ON v.id = f.volume_id LEFT JOIN analysis a ON a.file_id = f.id
         WHERE f.missing = 0 AND f.quarantined = 0 AND v.online = 1",
    )?;
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        let vid: i64 = r.get(1)?;
        let i = match at.get(&vid) {
            Some(i) => *i,
            None => {
                vols.push(Vol { id: vid, mount: r.get(5)?, files: Vec::new() });
                at.insert(vid, vols.len() - 1);
                vols.len() - 1
            }
        };
        let kind: String = r.get(6)?;
        vols[i].files.push(Rec { id: r.get(0)?, rel: r.get(2)?, size: r.get(3)?, key: r.get(4)?, video: kind == "video", taken: r.get(7)?, bad: r.get(8)? });
    }
    for v in &mut vols {
        v.files.sort_unstable_by(|a, b| a.rel.cmp(&b.rel));
    }
    vols.sort_by_key(|v| v.id);
    Ok(vols)
}

/// Folders above `d`, nearest first (`a/b/c` → `a/b`, `a`).
fn ancestors(d: &str) -> impl Iterator<Item = &str> {
    let mut cur = d;
    std::iter::from_fn(move || {
        let i = cur.rfind('/')?;
        cur = &cur[..i];
        Some(cur)
    })
}

fn nested(a: &str, b: &str) -> bool {
    a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

/// How many independent hashes a folder is sketched with (see `near_in`).
const SKETCH: usize = 16;

/// Hashes of one file's content: a 128-bit value for a folder's signature (their sum), and
/// `SKETCH` independent 64-bit values for finding folders that share most of their files (the
/// smallest of each over a folder: two folders agree on one with the chance of their overlap,
/// so folders 80 % alike are missed on all sixteen about once in a hundred million pairs).
fn hashes(key: &str, size: i64) -> (u128, [u64; SKETCH]) {
    let mut h = blake3::Hasher::new();
    h.update(format!("{size}:{key}").as_bytes());
    let mut b = [0u8; 16 + SKETCH * 8];
    h.finalize_xof().fill(&mut b);
    let mut lo = [0u8; 16];
    lo.copy_from_slice(&b[..16]);
    let mut m = [0u64; SKETCH];
    for (i, v) in m.iter_mut().enumerate() {
        let mut w = [0u8; 8];
        w.copy_from_slice(&b[16 + i * 8..24 + i * 8]);
        *v = u64::from_le_bytes(w);
    }
    (u128::from_le_bytes(lo), m)
}

/// A folder's content as a sorted list — what "the same" finally means.
fn content<'a>(files: impl Iterator<Item = &'a Rec>) -> Vec<(&'a str, i64)> {
    let mut l: Vec<(&str, i64)> = files.map(|r| (r.key.as_deref().unwrap_or(""), r.size)).collect();
    l.sort_unstable();
    l
}

/// The same, of the files that have a fingerprint (a file without one is no copy of anything).
fn keyed<'a>(files: impl Iterator<Item = &'a Rec>) -> Vec<(&'a str, i64)> {
    let mut l: Vec<(&str, i64)> = files.filter_map(|r| Some((r.key.as_deref()?, r.size))).collect();
    l.sort_unstable();
    l
}

/// How many entries two sorted content lists share (each file counted once), and their size.
fn overlap(a: &[(&str, i64)], b: &[(&str, i64)]) -> (u64, i64) {
    let (mut i, mut j, mut n, mut bytes) = (0, 0, 0u64, 0i64);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                n += 1;
                bytes += a[i].1;
                i += 1;
                j += 1;
            }
        }
    }
    (n, bytes)
}

/// Names and places inside the folder (lower-cased) with the content at each: two folders with
/// the same value were copied as they are; a different value means files were renamed or moved.
fn names_key(v: &Vol, dir: &str) -> String {
    let mut l: Vec<(String, &str)> = v.subtree(dir).iter().map(|r| (r.rel[dir.len() + 1..].to_lowercase(), r.key.as_deref().unwrap_or(""))).collect();
    l.sort_unstable();
    let mut h = blake3::Hasher::new();
    for (n, k) in &l {
        h.update(n.as_bytes());
        h.update(&[0]);
        h.update(k.as_bytes());
        h.update(&[0]);
    }
    h.finalize().to_hex()[..16].to_string()
}

/// Each file of the copy folder with its twin in the folder that stays: the file at the same
/// place if it is the same content, else any file with that content (each twin used once).
/// Files without a fingerprint have no twin.
fn twins<'a>(k: &[&'a Rec], kd: &str, c: &[&'a Rec], cd: &str) -> HashMap<i64, &'a Rec> {
    let mut pool: HashMap<(&str, i64), Vec<&Rec>> = HashMap::new();
    for r in k {
        if let Some(key) = r.key.as_deref() {
            pool.entry((key, r.size)).or_default().push(r);
        }
    }
    let inner = |r: &Rec, d: &str| r.rel[d.len() + 1..].to_lowercase();
    let mut twin: HashMap<i64, &Rec> = HashMap::new();
    for r in c {
        let Some(key) = r.key.as_deref() else { continue };
        let name = inner(r, cd);
        if let Some(l) = pool.get_mut(&(key, r.size)) {
            if let Some(i) = l.iter().position(|t| inner(t, kd) == name) {
                twin.insert(r.id, l.swap_remove(i));
            }
        }
    }
    for r in c {
        let Some(key) = r.key.as_deref() else { continue };
        if let std::collections::hash_map::Entry::Vacant(slot) = twin.entry(r.id) {
            if let Some(t) = pool.get_mut(&(key, r.size)).and_then(|l| l.pop()) {
                slot.insert(t);
            }
        }
    }
    twin
}

/// Of the copy's files that have no twin: those with a file of the folder that stays at the
/// same place (path inside the folder, any letter case) that no copy file is the twin of — the
/// same photo by name, not by content. Copy file id → that file.
fn by_place<'a>(k: &[&'a Rec], kd: &str, c: &[&'a Rec], cd: &str, twin: &HashMap<i64, &'a Rec>) -> HashMap<i64, &'a Rec> {
    let used: HashSet<i64> = twin.values().map(|t| t.id).collect();
    let free: HashMap<String, &Rec> = k.iter().filter(|r| !used.contains(&r.id)).map(|r| (r.rel[kd.len() + 1..].to_lowercase(), *r)).collect();
    c.iter().filter(|r| !twin.contains_key(&r.id)).filter_map(|r| free.get(&r.rel[cd.len() + 1..].to_lowercase()).map(|t| (r.id, *t))).collect()
}

// ---------------------------------------------------------------------------------------------
// Find
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct FolderInfo {
    pub volume_id: i64,
    pub rel_dir: String,
    pub abs_path: String,
    /// When the folder was made (a copy made in Explorer is as old as the copying), if the
    /// drive tells.
    pub created: Option<i64>,
    pub modified: Option<i64>,
    /// Inside a folder the user marked "always keep": never offered for deletion.
    pub keep_rule: bool,
    /// Already filed in the library.
    pub filed: bool,
    /// Equal between two folders whose files also have the same names and places.
    pub names: String,
    /// What is inside (sub-folders included): photos, videos, their size, the folders below,
    /// and when the first and the last were taken (wall-clock).
    pub photos: u64,
    pub videos: u64,
    pub bytes: i64,
    pub subfolders: u64,
    pub taken_from: Option<i64>,
    pub taken_to: Option<i64>,
    /// Photos in it the analysis could not open (likely damaged).
    pub unreadable: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct FolderSet {
    /// The folder suggested to stay.
    pub keep: FolderInfo,
    /// Why that one: "keep_rule" | "filed" | "oldest" | "".
    pub keep_why: String,
    pub copies: Vec<FolderInfo>,
    /// Photos and videos in each of the folders, and their size.
    pub files: u64,
    pub bytes: i64,
    /// A few of the photos (of the folder that stays), for thumbnails.
    pub sample: Vec<i64>,
}

/// A folder almost the same as the base folder of its set.
#[derive(Clone, Debug, Serialize)]
pub struct NearFolder {
    pub info: FolderInfo,
    /// Files both have (by content), and their size: what goes from this folder.
    pub shared: u64,
    pub shared_bytes: i64,
    /// Files only the base folder has / only this folder has (not counting `differs`).
    pub only_base: u64,
    pub only_here: u64,
    /// The same photos — `shared` and `differs` — over the files of the bigger of the two.
    pub ratio: f64,
    /// Photos both have under the same name at the same place, with different content — one of
    /// the two is damaged, cut short or edited. These are the ones to look at.
    pub differs: u64,
    /// Folders inside this one that are also listed as 100 % copies (removing what is shared
    /// here settles those too), and the part of `shared_bytes` not already counted there.
    pub also_listed: u64,
    pub extra_bytes: i64,
}

/// Folders that are almost the same: the biggest one and the others compared with it.
#[derive(Clone, Debug, Serialize)]
pub struct NearSet {
    pub base: FolderInfo,
    pub others: Vec<NearFolder>,
    pub sample: Vec<i64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Found {
    pub sets: Vec<FolderSet>,
    /// Folders that share at least the asked share of their files without being identical.
    pub near: Vec<NearSet>,
    /// Files not fingerprinted yet that could be copies (another file has their size): folders
    /// holding them are not judged until they are read. (A file whose size is unique is never
    /// read and never a copy.)
    pub unread: u64,
    /// The only drive looked at (the library drive), if one is chosen: its mount path.
    pub drive: Option<String>,
}

/// The last list found, kept until the files change: the list is asked for whenever its page
/// opens, and a library does not change between two visits. One computation at a time — a
/// second caller waits for the first and takes its result.
#[derive(Default)]
pub struct FoundCache {
    last: std::sync::Mutex<Option<(u64, u64, Found)>>,
}

impl FoundCache {
    /// The list for `near` as of generation `gen` (a number the caller raises whenever files
    /// changed): from memory if that is what was computed last, else from `compute`.
    pub fn get(&self, gen: u64, near: f64, compute: impl FnOnce() -> Result<Found>) -> Result<Found> {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((g, n, found)) = last.as_ref() {
            if *g == gen && *n == near.to_bits() {
                return Ok(found.clone());
            }
        }
        let found = compute()?;
        *last = Some((gen, near.to_bits(), found.clone()));
        Ok(found)
    }
}

/// "거의 같은 폴더": at least this share of the bigger folder's files is in both.
pub const NEAR: f64 = 0.95;

/// Order among identical folders (lowest stays): see `info`.
type Rank = (u8, bool, i64, i64, i64);
/// A folder's sorted content, and the folders that have exactly it.
type SameContent<'a> = (Vec<(&'a str, i64)>, Vec<&'a str>);

#[derive(Clone, Copy)]
struct Agg {
    count: u64,
    bytes: i64,
    sum: u128,
    unknown: bool,
    /// Smallest of each of the sketch hashes over the fingerprinted files.
    mins: [u64; SKETCH],
    keyed: u64,
}

impl Default for Agg {
    fn default() -> Self {
        Self { count: 0, bytes: 0, sum: 0, unknown: false, mins: [u64::MAX; SKETCH], keyed: 0 }
    }
}

impl Agg {
    fn add(&mut self, o: &Agg) {
        self.count += o.count;
        self.bytes += o.bytes;
        self.sum = self.sum.wrapping_add(o.sum);
        self.unknown |= o.unknown;
        self.keyed += o.keyed;
        for (m, other) in self.mins.iter_mut().zip(o.mins) {
            *m = (*m).min(other);
        }
    }
}

/// Every folder's whole subtree in numbers: each folder's own files, added up into every
/// folder above it.
fn totals(v: &Vol) -> HashMap<&str, Agg> {
    let mut direct: HashMap<&str, Agg> = HashMap::new();
    for r in &v.files {
        let Some(i) = r.rel.rfind('/') else { continue };
        let a = direct.entry(&r.rel[..i]).or_default();
        a.count += 1;
        a.bytes += r.size;
        match &r.key {
            Some(k) => {
                let (sum, mins) = hashes(k, r.size);
                a.sum = a.sum.wrapping_add(sum);
                a.keyed += 1;
                for (m, h) in a.mins.iter_mut().zip(mins) {
                    *m = (*m).min(h);
                }
            }
            None => a.unknown = true,
        }
    }
    let mut total: HashMap<&str, Agg> = HashMap::new();
    for (dir, a) in &direct {
        for d in std::iter::once(*dir).chain(ancestors(dir)) {
            total.entry(d).or_default().add(a);
        }
    }
    total
}

/// Every set of identical folders and of almost identical folders, biggest first. Reads the
/// index, and the disk only for the dates of the folders it reports.
pub fn find(conn: &Connection) -> Result<Found> {
    find_with(conn, NEAR)
}

/// `find` with another bar for "almost the same" (`near`: 0.95 = 95 % of the bigger folder).
/// A lower bar lists more folders — and more that are no copies at all, only folders sharing
/// photos; what is removed stays the same either way: files the other folder really has.
pub fn find_with(conn: &Connection, near: f64) -> Result<Found> {
    let near = near.clamp(0.5, 0.999);
    let vols = load(conn)?;
    let rules = DupRules::load(conn)?;
    let lib = library::location(conn);
    let mut out = Found::default();
    let mut sizes: HashMap<i64, u32> = HashMap::new();
    for r in vols.iter().flat_map(|v| v.files.iter()) {
        *sizes.entry(r.size).or_default() += 1;
    }
    for v in &vols {
        if lib.as_ref().is_some_and(|l| l.0 != v.id) {
            continue;
        }
        if lib.is_some() {
            out.drive = Some(v.mount.clone());
        }
        out.unread += v.files.iter().filter(|r| r.key.is_none() && sizes.get(&r.size).is_some_and(|n| *n > 1)).count() as u64;
        let total = totals(v);
        let covered = find_in(v, &total, &rules, lib.as_ref(), &mut out.sets);
        near_in(v, &total, &covered, &rules, lib.as_ref(), near, &mut out.near);
    }
    out.sets.sort_by(|a, b| {
        (b.bytes * b.copies.len() as i64).cmp(&(a.bytes * a.copies.len() as i64)).then_with(|| a.keep.abs_path.cmp(&b.keep.abs_path))
    });
    let gain = |n: &NearSet| n.others.iter().map(|o| o.shared_bytes).sum::<i64>();
    out.near.sort_by(|a, b| gain(b).cmp(&gain(a)).then_with(|| a.base.abs_path.cmp(&b.base.abs_path)));
    Ok(out)
}

/// The sets of identical folders of one drive. Returns the folders of those sets: `true` for
/// the one that stays, `false` for a copy that goes.
fn find_in<'a>(v: &'a Vol, total: &HashMap<&'a str, Agg>, rules: &DupRules, lib: Option<&(i64, String)>, out: &mut Vec<FolderSet>) -> HashMap<&'a str, bool> {
    let mut by_sig: HashMap<(u64, i64, u128), Vec<&str>> = HashMap::new();
    for (d, a) in total {
        if !a.unknown && a.count > 0 {
            by_sig.entry((a.count, a.bytes, a.sum)).or_default().push(d);
        }
    }
    // Candidates: equal signature, then really the same content. A folder that only wraps
    // another (`백업` holding nothing but `백업/제주도`) is that folder: the inner one is taken,
    // so copies are compared where their files really are.
    let mut cands: Vec<(u64, i64, Vec<&str>)> = Vec::new();
    for ((count, bytes, _), dirs) in by_sig {
        if dirs.len() < 2 {
            continue;
        }
        let members: HashSet<&str> = dirs.iter().copied().collect();
        let wrappers: HashSet<&str> = dirs.iter().flat_map(|d| ancestors(d)).filter(|a| members.contains(a)).collect();
        let mut own: Vec<&str> = dirs.into_iter().filter(|d| !wrappers.contains(d)).collect();
        if own.len() < 2 {
            continue;
        }
        own.sort_unstable();
        let mut classes: Vec<SameContent> = Vec::new();
        for d in own {
            let list = content(v.subtree(d).iter());
            match classes.iter_mut().find(|(l, _)| *l == list) {
                Some((_, ds)) => ds.push(d),
                None => classes.push((list, vec![d])),
            }
        }
        cands.extend(classes.into_iter().filter(|(_, ds)| ds.len() >= 2).map(|(_, ds)| (count, bytes, ds)));
    }
    // Bigger sets first: a folder inside a copy that goes anyway is not listed again.
    cands.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then_with(|| a.2.cmp(&b.2)));
    let mut covered: HashMap<&str, bool> = HashMap::new();
    for (count, bytes, dirs) in cands {
        let mut infos: Vec<(&str, FolderInfo, Rank)> = dirs
            .into_iter()
            .filter(|d| ancestors(d).find_map(|a| covered.get(a)).copied().unwrap_or(true))
            .filter_map(|d| info(v, d, rules, lib).map(|(i, rank)| (d, i, rank)))
            .collect();
        if infos.len() < 2 {
            continue;
        }
        infos.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(b.0)));
        for (i, (d, _, _)) in infos.iter().enumerate() {
            covered.insert(d, i == 0);
        }
        let sample: Vec<i64> = v.subtree(infos[0].0).iter().take(6).map(|r| r.id).collect();
        let mut it = infos.into_iter().map(|(_, i, _)| i);
        let Some(keep) = it.next() else { continue };
        let keep_why = if keep.keep_rule {
            "keep_rule"
        } else if keep.filed {
            "filed"
        } else if keep.created.is_some() {
            "oldest"
        } else {
            ""
        };
        out.push(FolderSet { keep, keep_why: keep_why.into(), copies: it.collect(), files: count, bytes, sample });
    }
    covered
}

/// The sets of almost identical folders of one drive: pairs sharing at least `near` of the
/// bigger one's files. Candidates come from the smallest sketch hashes (folders that alike
/// agree on at least one, all but never missing a pair); each candidate is then counted
/// exactly. Only the outermost pairs are kept, grouped around the biggest folder.
#[allow(clippy::too_many_arguments)]
fn near_in<'a>(v: &'a Vol, total: &HashMap<&'a str, Agg>, covered: &HashMap<&'a str, bool>, rules: &DupRules, lib: Option<&(i64, String)>, near: f64, out: &mut Vec<NearSet>) {
    let min_files = (1.0 / (1.0 - near)).round() as u64; // below this, one file apart is already too far
    // A folder holding nothing but one sub-folder is that sub-folder.
    let wrappers: HashSet<&str> = total.iter().filter_map(|(d, a)| ancestors(d).next().filter(|p| total.get(p).is_some_and(|t| t.count == a.count))).collect();
    let goes = |d: &str| std::iter::once(d).chain(ancestors(d)).find_map(|a| covered.get(a)).is_some_and(|stays| !*stays);
    let mut buckets: HashMap<(u8, u64), Vec<&str>> = HashMap::new();
    // A folder whose every file is in another one is listed whatever its share of that folder
    // (nothing of it would be lost): those may be small.
    const CONTAINED_MIN: u64 = 3;
    for (d, a) in total {
        if a.count < min_files.min(CONTAINED_MIN) || a.keyed == 0 || wrappers.contains(d) || goes(d) {
            continue;
        }
        for (i, m) in a.mins.iter().enumerate() {
            buckets.entry((i as u8, *m)).or_default().push(d);
        }
    }
    let count = |d: &str| total.get(d).map_or(0, |a| a.count);
    let mut pairs: HashSet<(&str, &str)> = HashSet::new();
    for (_, mut ds) in buckets {
        if ds.len() < 2 {
            continue;
        }
        ds.sort_unstable_by_key(|d| (count(d), *d));
        for i in 0..ds.len() {
            // (Not endlessly many: a file that is in hundreds of folders says little.)
            for j in (i + 1..ds.len()).take(200) {
                let (a, b) = (ds[i], ds[j]);
                if !nested(a, b) {
                    pairs.insert(if a < b { (a, b) } else { (b, a) });
                }
            }
        }
    }
    let mut lists: HashMap<&str, Vec<(&str, i64)>> = HashMap::new();
    let mut alike: Vec<(&str, &str, u64, i64)> = Vec::new();
    for (a, b) in pairs {
        for d in [a, b] {
            lists.entry(d).or_insert_with(|| keyed(v.subtree(d).iter()));
        }
        let (shared, bytes) = overlap(&lists[a], &lists[b]);
        let (ca, cb) = (count(a), count(b));
        let identical = shared == ca && shared == cb;
        let most = ca.max(cb) as f64;
        // A photo under the same name at the same place counts as the same photo even when its
        // content differs (damaged, cut short): such a pair is what the list is for, and must
        // not drop out of it because of the very files worth a look. (Counted only for pairs
        // that would otherwise just miss the bar.)
        let mut named = 0u64;
        if !identical && ca.min(cb) >= min_files && (shared as f64) < near * most - 1e-9 && shared as f64 >= (near - 0.3) * most {
            let (kr, cr): (Vec<&Rec>, Vec<&Rec>) = (v.subtree(a).iter().collect(), v.subtree(b).iter().collect());
            named = by_place(&kr, a, &cr, b, &twins(&kr, a, &cr, b)).len() as u64;
        }
        let alike_enough = ca.min(cb) >= min_files && (shared + named) as f64 >= near * most - 1e-9;
        // (A folder that is one of a 100 % set is settled there: its copy inside the other
        // folder is what goes, not the folder itself.)
        let smaller = if ca <= cb { a } else { b };
        let contained = shared == ca.min(cb) && shared >= CONTAINED_MIN && !covered.contains_key(smaller);
        if !identical && (alike_enough || contained) {
            alike.push((a, b, shared, bytes));
        }
    }
    // Outermost pairs only: `A` ~ `B` covers `A/파리` ~ `B/파리` (and `A/파리` ~ `B`).
    alike.sort_by(|x, y| count(y.0).max(count(y.1)).cmp(&count(x.0).max(count(x.1))).then_with(|| (x.0, x.1).cmp(&(y.0, y.1))));
    let mut accepted: HashSet<(&str, &str)> = HashSet::new();
    let mut partners: HashMap<&str, Vec<(&str, u64, i64)>> = HashMap::new();
    for (a, b, shared, bytes) in alike {
        let inside = std::iter::once(a).chain(ancestors(a)).any(|x| std::iter::once(b).chain(ancestors(b)).any(|y| accepted.contains(&if x < y { (x, y) } else { (y, x) })));
        if inside {
            continue;
        }
        accepted.insert((a, b));
        partners.entry(a).or_default().push((b, shared, bytes));
        partners.entry(b).or_default().push((a, shared, bytes));
    }
    // The biggest folder of each group is its base (then the usual order); a folder appears once.
    let mut nodes: Vec<(&str, FolderInfo, Rank)> = partners.keys().filter_map(|d| info(v, d, rules, lib).map(|(i, r)| (*d, i, r))).collect();
    nodes.sort_by(|x, y| count(y.0).cmp(&count(x.0)).then_with(|| x.2.cmp(&y.2)).then_with(|| x.0.cmp(y.0)));
    let infos: HashMap<&str, FolderInfo> = nodes.iter().map(|(d, i, _)| (*d, i.clone())).collect();
    let mut used: HashSet<&str> = HashSet::new();
    for (d, base, _) in &nodes {
        if used.contains(d) {
            continue;
        }
        let mut others: Vec<NearFolder> = partners[d]
            .iter()
            .filter(|(o, _, _)| !used.contains(o))
            .filter_map(|(o, shared, bytes)| {
                let info = infos.get(o)?.clone();
                let (cb, co) = (count(d), count(o));
                // Folders of 100 % sets inside this one (the outermost of them, for the size).
                let inside: Vec<&str> = covered.keys().copied().filter(|c| c.starts_with(&format!("{o}/"))).collect();
                let listed: i64 = inside.iter().filter(|c| !ancestors(c).any(|a| a.len() > o.len() && covered.contains_key(a))).map(|c| total.get(c).map_or(0, |a| a.bytes)).sum();
                let (kr, cr): (Vec<&Rec>, Vec<&Rec>) = (v.subtree(d).iter().collect(), v.subtree(o).iter().collect());
                let differs = by_place(&kr, d, &cr, o, &twins(&kr, d, &cr, o)).len() as u64;
                Some(NearFolder {
                    info,
                    differs,
                    shared: *shared,
                    shared_bytes: *bytes,
                    only_base: (cb - shared).saturating_sub(differs),
                    only_here: (co - shared).saturating_sub(differs),
                    ratio: (*shared + differs) as f64 / cb.max(co) as f64,
                    also_listed: inside.len() as u64,
                    extra_bytes: (*bytes - listed).max(0),
                })
            })
            .collect();
        if others.is_empty() {
            continue;
        }
        others.sort_by(|x, y| y.shared.cmp(&x.shared).then_with(|| x.info.rel_dir.cmp(&y.info.rel_dir)));
        used.insert(d);
        used.extend(others.iter().filter_map(|o| infos.get_key_value(o.info.rel_dir.as_str()).map(|(k, _)| *k)));
        let sample: Vec<i64> = v.subtree(d).iter().take(6).map(|r| r.id).collect();
        out.push(NearSet { base: base.clone(), others, sample });
    }
}

/// A folder as shown, and its rank among identical folders (lowest stays): under a keep rule,
/// filed in the library, then the oldest — made first, else changed first, else indexed first.
fn info(v: &Vol, dir: &str, rules: &DupRules, lib: Option<&(i64, String)>) -> Option<(FolderInfo, Rank)> {
    let abs = v.abs(dir);
    let md = fs::metadata(&abs).ok().filter(|m| m.is_dir())?;
    let ms = |t: std::io::Result<std::time::SystemTime>| t.ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64);
    let (created_ms, modified_ms) = (ms(md.created()), ms(md.modified()));
    let (created, modified) = (created_ms.map(|t| t / 1000), modified_ms.map(|t| t / 1000));
    let abs_s = abs.to_string_lossy().into_owned();
    let class = rules.class_of_path(&abs_s);
    let filed = lib.is_some_and(|(lv, root)| *lv == v.id && (root.is_empty() || dir == root || dir.starts_with(&format!("{root}/"))));
    let files = v.subtree(dir);
    let first_id = files.iter().map(|r| r.id).min().unwrap_or(i64::MAX);
    let videos = files.iter().filter(|r| r.video).count() as u64;
    let below: HashSet<&str> = files.iter().filter_map(|r| r.rel.rfind('/').map(|i| &r.rel[..i])).flat_map(|d| std::iter::once(d).chain(ancestors(d))).filter(|d| d.len() > dir.len()).collect();
    let rank = (class, !filed, created_ms.unwrap_or(i64::MAX), modified_ms.unwrap_or(i64::MAX), first_id);
    Some((
        FolderInfo {
            volume_id: v.id,
            rel_dir: dir.to_string(),
            abs_path: abs_s,
            created,
            modified,
            keep_rule: class == 0,
            filed,
            names: names_key(v, dir),
            photos: files.len() as u64 - videos,
            videos,
            bytes: files.iter().map(|r| r.size).sum(),
            subfolders: below.len() as u64,
            taken_from: files.iter().filter_map(|r| r.taken).min(),
            taken_to: files.iter().filter_map(|r| r.taken).max(),
            unreadable: files.iter().filter(|r| r.bad).count() as u64,
        },
        rank,
    ))
}

// ---------------------------------------------------------------------------------------------
// Look inside
// ---------------------------------------------------------------------------------------------

/// The live files of one folder (sub-folders included), by path, and the drive's mount.
fn rows_in(conn: &Connection, f: &FolderRef) -> Result<(String, Vec<Rec>)> {
    let f = clean(f);
    let mount: String = conn.query_row("SELECT mount FROM volumes WHERE id = ?", [f.volume_id], |r| r.get(0))?;
    let mut st = conn.prepare(
        "SELECT f.id, f.rel_path, f.size, COALESCE(f.prehash, f.hash), f.kind, f.taken_at,
                (a.file_id IS NOT NULL AND a.dhash IS NULL AND f.kind = 'photo')
         FROM files f LEFT JOIN analysis a ON a.file_id = f.id
         WHERE f.volume_id = ?1 AND f.rel_path >= ?2 AND f.rel_path < ?3 AND f.missing = 0 AND f.quarantined = 0 ORDER BY f.rel_path",
    )?;
    // Everything that starts with "dir/": up to the next character after '/'.
    let rows = st
        .query_map(params![f.volume_id, format!("{}/", f.rel_dir), format!("{}0", f.rel_dir)], |r| {
            let kind: String = r.get(4)?;
            Ok(Rec { id: r.get(0)?, rel: r.get(1)?, size: r.get(2)?, key: r.get(3)?, video: kind == "video", taken: r.get(5)?, bad: r.get(6)? })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((mount, rows))
}

/// The files of a folder, sub-folder by sub-folder ("" = the folder itself).
#[derive(Clone, Debug, Serialize)]
pub struct FolderFiles {
    pub dir: String,
    pub ids: Vec<i64>,
}

pub fn files_in(conn: &Connection, folder: &FolderRef) -> Result<Vec<FolderFiles>> {
    let (_, rows) = rows_in(conn, folder)?;
    let root = clean(folder).rel_dir;
    let mut by_dir: std::collections::BTreeMap<String, Vec<i64>> = std::collections::BTreeMap::new();
    for r in &rows {
        let inner = &r.rel[root.len() + 1..];
        by_dir.entry(inner.rfind('/').map_or(String::new(), |i| inner[..i].to_string())).or_default().push(r.id);
    }
    Ok(by_dir.into_iter().map(|(dir, ids)| FolderFiles { dir, ids }).collect())
}

/// Two folders side by side: the files both have (as pairs: in the folder that stays, in the
/// copy), and the files only one of them has.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Compare {
    pub shared: Vec<(i64, i64)>,
    pub only_keep: Vec<i64>,
    pub only_copy: Vec<i64>,
}

pub fn compare(conn: &Connection, keep: &FolderRef, copy: &FolderRef) -> Result<Compare> {
    let (_, k) = rows_in(conn, keep)?;
    let (_, c) = rows_in(conn, copy)?;
    let (kd, cd) = (clean(keep).rel_dir, clean(copy).rel_dir);
    let (kr, cr): (Vec<&Rec>, Vec<&Rec>) = (k.iter().collect(), c.iter().collect());
    let twin = twins(&kr, &kd, &cr, &cd);
    let mut out = Compare::default();
    let mut paired: HashSet<i64> = HashSet::new();
    for r in &c {
        match twin.get(&r.id) {
            Some(t) => {
                paired.insert(t.id);
                out.shared.push((t.id, r.id));
            }
            None => out.only_copy.push(r.id),
        }
    }
    out.only_keep = k.iter().filter(|r| !paired.contains(&r.id)).map(|r| r.id).collect();
    Ok(out)
}

/// What else is in a folder on disk: files the index does not hold (documents, sidecars) —
/// what stays behind when the photos go.
#[derive(Clone, Debug, Default, Serialize)]
pub struct FolderProps {
    pub other_files: u64,
    pub other_bytes: u64,
    pub examples: Vec<String>,
}

pub fn props(conn: &Connection, folder: &FolderRef) -> Result<FolderProps> {
    let (mount, rows) = rows_in(conn, folder)?;
    let root = volumes::join_rel(Path::new(&mount), &clean(folder).rel_dir);
    let known: HashSet<PathBuf> = rows.iter().map(|r| volumes::join_rel(Path::new(&mount), &r.rel)).collect();
    let mut out = FolderProps::default();
    let mut stack = vec![root.clone()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(t) if t.is_file() && !known.contains(&p) => {
                    out.other_files += 1;
                    out.other_bytes += e.metadata().map_or(0, |m| m.len());
                    if out.examples.len() < 5 {
                        out.examples.push(p.strip_prefix(&root).unwrap_or(&p).to_string_lossy().into_owned());
                    }
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

/// One folder's file at one place of the walk-through.
#[derive(Clone, Debug, Serialize)]
pub struct AlignFile {
    pub id: i64,
    pub name: String,
    pub size: i64,
    /// The same content as the first file of its row (the original's, where it has one).
    pub same: bool,
    /// A photo the analysis could not open: likely damaged.
    pub unreadable: bool,
    pub video: bool,
}

/// One photo across the folders of a set: the original's file first, then each copy's (`None`
/// where a folder has none). `kind`: "same" (all have it, all identical), "differs" (some file
/// here is not the same content), "only" (identical where present, but a folder lacks it).
#[derive(Clone, Debug, Serialize)]
pub struct AlignRow {
    pub path: String,
    pub files: Vec<Option<AlignFile>>,
    pub kind: String,
}

/// The folders of a set photo by photo, for looking at them side by side. A copy's file sits
/// in the row of the original's file with the same content, else of the file at the same place
/// (same photo by name, different content), else in a row of its own.
pub fn align(conn: &Connection, keep: &FolderRef, copies: &[FolderRef]) -> Result<Vec<AlignRow>> {
    let (_, k) = rows_in(conn, keep)?;
    let kd = clean(keep).rel_dir;
    let n = copies.len() + 1;
    let file = |r: &Rec, same: bool| AlignFile { id: r.id, name: r.rel.rsplit('/').next().unwrap_or("").to_string(), size: r.size, same, unreadable: r.bad, video: r.video };
    let mut rows: Vec<AlignRow> = k
        .iter()
        .map(|r| {
            let mut files = vec![None; n];
            files[0] = Some(file(r, true));
            AlignRow { path: r.rel[kd.len() + 1..].to_string(), files, kind: String::new() }
        })
        .collect();
    let at: HashMap<i64, usize> = k.iter().enumerate().map(|(i, r)| (r.id, i)).collect();
    let kr: Vec<&Rec> = k.iter().collect();
    // Rows only copies have, by place; and the content of the first file put in each.
    let mut extra: HashMap<String, (usize, Option<String>, i64)> = HashMap::new();
    for (j, c_ref) in copies.iter().enumerate() {
        let (_, c) = rows_in(conn, c_ref)?;
        let cd = clean(c_ref).rel_dir;
        let cr: Vec<&Rec> = c.iter().collect();
        let twin = twins(&kr, &kd, &cr, &cd);
        let named = by_place(&kr, &kd, &cr, &cd, &twin);
        for r in &c {
            if let Some(t) = twin.get(&r.id) {
                rows[at[&t.id]].files[j + 1] = Some(file(r, true));
            } else if let Some(t) = named.get(&r.id) {
                rows[at[&t.id]].files[j + 1] = Some(file(r, false));
            } else {
                let place = r.rel[cd.len() + 1..].to_string();
                let (i, key, size) = extra.entry(place.to_lowercase()).or_insert_with(|| {
                    rows.push(AlignRow { path: place.clone(), files: vec![None; n], kind: String::new() });
                    (rows.len() - 1, r.key.clone(), r.size)
                });
                rows[*i].files[j + 1] = Some(file(r, r.key.is_some() && *key == r.key && *size == r.size));
            }
        }
    }
    for row in &mut rows {
        // The same content opens — or does not — the same way in every folder: one "could not
        // be opened" among identical files stands for all of them.
        if row.files.iter().flatten().any(|f| f.same && f.unreadable) {
            for f in row.files.iter_mut().flatten().filter(|f| f.same) {
                f.unreadable = true;
            }
        }
        let all_same = row.files.iter().flatten().all(|f| f.same);
        let all_there = row.files.iter().all(|f| f.is_some());
        row.kind = if !all_same { "differs" } else if !all_there { "only" } else { "same" }.into();
    }
    rows.sort_by_cached_key(|r| r.path.to_lowercase());
    Ok(rows)
}

/// Where two files first differ.
#[derive(Clone, Debug, Default, Serialize)]
pub struct FirstDifference {
    /// Offset of the first byte that differs (the shorter length if one is the start of the
    /// other); `None` if the two are the same.
    pub at: Option<u64>,
    pub size_a: u64,
    pub size_b: u64,
    /// Of two files that differ: which one looks whole and which damaged — cut short, or with
    /// empty stretches the other does not have. Both `None`: cannot be told from the bytes.
    pub whole_a: Option<bool>,
    pub whole_b: Option<bool>,
}

pub fn first_difference(conn: &Connection, a: i64, b: i64) -> Result<FirstDifference> {
    first_difference_with(conn, a, b, &|| false)
}

/// `first_difference` that gives up (with an error) once `cancelled` says so — asked every
/// megabyte. The user has moved on to another photo: reading this pair on is wasted disk time.
pub fn first_difference_with(conn: &Connection, a: i64, b: i64, cancelled: &dyn Fn() -> bool) -> Result<FirstDifference> {
    use std::io::Read;
    let path = |id: i64| -> Result<PathBuf> { Ok(db::get_file(conn, id)?.ok_or_else(|| anyhow::anyhow!("파일을 찾을 수 없습니다"))?.abs_path()) };
    let (pa, pb) = (path(a)?, path(b)?);
    let mut out = FirstDifference { at: None, size_a: fs::metadata(&pa)?.len(), size_b: fs::metadata(&pb)?.len(), ..Default::default() };
    let (mut fa, mut fb) = (fs::File::open(&pa)?, fs::File::open(&pb)?);
    let (mut ba, mut bb) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    let mut pos = 0u64;
    loop {
        let fill = |f: &mut fs::File, buf: &mut [u8]| -> std::io::Result<usize> {
            let mut n = 0;
            while n < buf.len() {
                match f.read(&mut buf[n..])? {
                    0 => break,
                    m => n += m,
                }
            }
            Ok(n)
        };
        if cancelled() {
            anyhow::bail!("cancelled");
        }
        let (x, y) = (fill(&mut fa, &mut ba)?, fill(&mut fb, &mut bb)?);
        let both = x.min(y);
        if let Some(i) = (0..both).find(|i| ba[*i] != bb[*i]) {
            out.at = Some(pos + i as u64);
            return Ok(out);
        }
        if x != y {
            out.at = Some(pos + both as u64);
            return Ok(out);
        }
        if x == 0 {
            return Ok(out);
        }
        pos += x as u64;
    }
}

/// How much of a file is empty: (4 KiB blocks of nothing but zero bytes, blocks in all).
/// Photos and videos are compressed, so such stretches hardly occur in them — they are what a
/// recovery tool or a broken copy leaves where data is missing. `None`: could not be read.
fn empty_blocks(conn: &Connection, id: i64) -> Option<(u64, u64)> {
    use std::io::Read;
    let f = db::get_file(conn, id).ok().flatten()?;
    let mut file = fs::File::open(f.abs_path()).ok()?;
    let mut buf = vec![0u8; 1 << 16];
    let (mut empty, mut blocks) = (0u64, 0u64);
    loop {
        let mut n = 0;
        while n < buf.len() {
            match file.read(&mut buf[n..]).ok()? {
                0 => break,
                m => n += m,
            }
        }
        for block in buf[..n].chunks(4096) {
            blocks += 1;
            if block.len() == 4096 && block.iter().all(|b| *b == 0) {
                empty += 1;
            }
        }
        if n < buf.len() {
            return Some((empty, blocks));
        }
    }
}

/// Of two files that differ, which one looks damaged: the one cut short (the other continues
/// where it ends), or the one with clearly more empty stretches. Judged between the two — the
/// same photo carries the same padding in both, so only what one of them lacks counts. (A
/// decoder is no judge here: it draws a damaged JPEG without complaint.)
fn judge(conn: &Connection, a: i64, b: i64, d: &mut FirstDifference) {
    if d.size_a != d.size_b && d.at == Some(d.size_a.min(d.size_b)) {
        (d.whole_a, d.whole_b) = (Some(d.size_a > d.size_b), Some(d.size_b > d.size_a));
        return;
    }
    let (Some((ea, na)), Some((eb, nb))) = (empty_blocks(conn, a), empty_blocks(conn, b)) else { return };
    let clearly = |more: u64, less: u64, blocks: u64| more > less + (blocks / 200).max(1);
    if clearly(ea, eb, na) {
        (d.whole_a, d.whole_b) = (Some(false), Some(true));
    } else if clearly(eb, ea, nb) {
        (d.whole_a, d.whole_b) = (Some(true), Some(false));
    }
}

/// Compare two files to the end, and if they differ although the index took them for the same
/// (same size, same first 64 KiB), teach the index: from then on they are different files
/// everywhere. Returns where they first differ.
pub fn check_pair(conn: &Connection, a: i64, b: i64) -> Result<FirstDifference> {
    check_pair_with(conn, a, b, &|| false)
}

/// `check_pair` that can be given up (see `first_difference_with`); given up, it has changed nothing.
pub fn check_pair_with(conn: &Connection, a: i64, b: i64, cancelled: &dyn Fn() -> bool) -> Result<FirstDifference> {
    let mut d = first_difference_with(conn, a, b, cancelled)?;
    if d.at.is_some() {
        judge(conn, a, b, &mut d);
        let key = |id: i64| -> Option<String> { conn.query_row("SELECT COALESCE(prehash, hash) FROM files WHERE id = ?", [id], |r| r.get::<_, Option<String>>(0)).ok().flatten() };
        if key(a).is_some() && key(a) == key(b) {
            tell_apart(conn, a, b)?;
        }
    }
    Ok(d)
}

// ---------------------------------------------------------------------------------------------
// Take the better file
// ---------------------------------------------------------------------------------------------

/// One photo decided between a copy folder and the folder that stays. The copy's file goes
/// into the folder that stays — in place of `keep_file` (the copy's is the better one), or added
/// (`None`: only the copy has it). With `discard` it is the other way round: `keep_file` is the
/// good one, and the copy's differing file goes to the quarantine.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Choice {
    pub keep_file: Option<i64>,
    pub copy_file: i64,
    #[serde(default)]
    pub discard: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SwapGroup {
    pub copy: FolderRef,
    pub choices: Vec<Choice>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SwapReport {
    pub batch_id: String,
    /// Files of the folder that stays replaced by the copy's (the old ones wait in the quarantine).
    pub replaced: u64,
    /// Files only a copy had, now in the folder that stays too.
    pub brought: u64,
    /// Copies' files that differed from the original's and were not wanted (in the quarantine).
    pub discarded: u64,
    pub failed: u64,
    pub errors: Vec<String>,
    pub cancelled: bool,
}

/// Copy the chosen files of copy folders into the folder that stays. A replaced file keeps its
/// name and place and goes to the quarantine (undoable); the user's marks on it pass to the new
/// file. The copies themselves stay where they are — removing the copy folder is another step,
/// and by then the two folders agree on these photos.
pub fn swap(db: &Db, keep: &FolderRef, groups: &[SwapGroup], stop: &StopFlag, progress: ProgressFn) -> Result<SwapReport> {
    let conn = db.conn()?;
    volumes::refresh(&conn)?;
    let keep = clean(keep);
    if library::location(&conn).is_some_and(|l| l.0 != keep.volume_id) {
        anyhow::bail!("라이브러리 드라이브가 아니라서 건드리지 않았습니다");
    }
    let total: u64 = groups.iter().map(|g| g.choices.len() as u64).sum();
    let replacing = groups.iter().any(|g| g.choices.iter().any(|c| c.keep_file.is_some() || c.discard));
    // With replaced files in its quarantine the batch is a deletion (it can be emptied); else
    // it only added files.
    let batch = journal::new_batch(&conn, if replacing { "delete" } else { "organize" }, "복사본의 사진을 원본 폴더로")?;
    let mut rep = SwapReport { batch_id: batch.clone(), ..Default::default() };
    let mut done = 0u64;
    'all: for g in groups {
        let cd = clean(&g.copy);
        for ch in &g.choices {
            if stop.is_stopped() {
                rep.cancelled = true;
                break 'all;
            }
            done += 1;
            progress(Progress::new("cleanup:swap", "copy", done, total, ""));
            match crate::guarded("사진 교체", || swap_one(&conn, &batch, &keep, &cd, ch)) {
                Ok(Done::Replaced) => rep.replaced += 1,
                Ok(Done::Brought) => rep.brought += 1,
                Ok(Done::Discarded) => rep.discarded += 1,
                Err(e) => {
                    rep.failed += 1;
                    if rep.errors.len() < 100 {
                        rep.errors.push(e.to_string());
                    }
                }
            }
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if rep.replaced > 0 {
        parts.push(format!("사진 {}장을 복사본 것으로 교체", rep.replaced));
    }
    if rep.brought > 0 {
        parts.push(format!("{}장을 원본 폴더로 가져옴", rep.brought));
    }
    if rep.discarded > 0 {
        parts.push(format!("원본과 다른 복사본 {}장을 격리", rep.discarded));
    }
    let what = if parts.is_empty() { "사진 고르기".to_string() } else { parts.join(", ") };
    conn.execute("UPDATE batches SET description = ? WHERE id = ?", params![what, batch])?;
    journal::finish_batch(&conn, &batch)?;
    if rep.replaced + rep.brought + rep.discarded == 0 {
        conn.execute("DELETE FROM batches WHERE id = ? AND op_count = 0", [&batch])?;
        rep.batch_id.clear();
    }
    Ok(rep)
}

enum Done {
    Replaced,
    Brought,
    Discarded,
}

fn swap_one(conn: &Connection, batch: &str, keep: &FolderRef, cd: &FolderRef, ch: &Choice) -> Result<Done> {
    let live = |id: i64| -> Result<db::FileRow> { db::get_file(conn, id)?.filter(|f| !f.quarantined && !f.missing && f.online).ok_or_else(|| anyhow::anyhow!("파일을 찾을 수 없습니다")) };
    let inside = |f: &db::FileRow, d: &FolderRef| f.volume_id == d.volume_id && f.rel_path.starts_with(&format!("{}/", d.rel_dir));
    let c = live(ch.copy_file)?;
    if cd.volume_id != keep.volume_id || !inside(&c, cd) || !c.abs_path().is_file() {
        anyhow::bail!("복사본 폴더의 파일이 아닙니다: {}", c.name);
    }
    if ch.discard {
        // Only with the original's file there to stay: a photo is never discarded on its own.
        let k = live(ch.keep_file.ok_or_else(|| anyhow::anyhow!("원본 폴더에 남는 파일이 없습니다"))?)?;
        if !inside(&k, keep) || !k.abs_path().is_file() {
            anyhow::bail!("원본 폴더의 파일이 아닙니다: {}", k.name);
        }
        journal::quarantine(conn, batch, &c)?;
        return Ok(Done::Discarded);
    }
    match ch.keep_file {
        Some(kid) => {
            let k = live(kid)?;
            if !inside(&k, keep) {
                anyhow::bail!("원본 폴더의 파일이 아닙니다: {}", k.name);
            }
            let (dst, rel) = (k.abs_path(), k.rel_path.clone());
            journal::quarantine_parked(conn, batch, &k)?;
            match journal::copy_indexed(conn, batch, &c, &dst, k.volume_id, &rel) {
                Ok(new) => {
                    conn.execute("INSERT OR IGNORE INTO labels(file_id, label, source) SELECT ?2, label, source FROM labels WHERE file_id = ?1 AND source = 'user'", params![k.id, new])?;
                    Ok(Done::Replaced)
                }
                Err(e) => {
                    // Nothing took its place: the old file goes back.
                    journal::restore_quarantined(conn, batch, k.id)?;
                    Err(e)
                }
            }
        }
        None => {
            // The same place inside the folder; a free name if that one is taken.
            let inner = &c.rel_path[cd.rel_dir.len() + 1..];
            let taken = |rel: &str| -> Result<bool> {
                let row: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM files WHERE volume_id = ? AND rel_path = ?)", params![keep.volume_id, rel], |r| r.get(0))?;
                Ok(row || volumes::join_rel(Path::new(&c.mount), rel).exists())
            };
            let mut rel = format!("{}/{inner}", keep.rel_dir);
            let mut n = 2;
            while taken(&rel)? {
                rel = match inner.rsplit_once('.') {
                    Some((stem, ext)) if !stem.ends_with('/') && !ext.contains('/') => format!("{}/{stem} ({n}).{ext}", keep.rel_dir),
                    _ => format!("{}/{inner} ({n})", keep.rel_dir),
                };
                n += 1;
            }
            journal::copy_indexed(conn, batch, &c, &volumes::join_rel(Path::new(&c.mount), &rel), keep.volume_id, &rel)?;
            Ok(Done::Brought)
        }
    }
}

/// Sets the user has looked through (by the folder that stays: "volume:folder").
pub fn reviewed(conn: &Connection) -> Result<Vec<String>> {
    use rusqlite::OptionalExtension;
    let v: Option<String> = conn.query_row("SELECT value FROM settings WHERE key = 'folders_reviewed'", [], |r| r.get(0)).optional()?;
    Ok(v.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
}

pub fn set_reviewed(conn: &Connection, key: &str, on: bool) -> Result<Vec<String>> {
    let mut all = reviewed(conn)?;
    all.retain(|k| k != key);
    if on {
        all.push(key.to_string());
    }
    // Old marks fall off the front.
    let from = all.len().saturating_sub(5000);
    let all = all.split_off(from);
    conn.execute("INSERT INTO settings(key, value) VALUES ('folders_reviewed', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value", [serde_json::to_string(&all)?])?;
    Ok(all)
}

// ---------------------------------------------------------------------------------------------
// Remove
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FolderRef {
    pub volume_id: i64,
    pub rel_dir: String,
}

/// One copy folder to remove, and the identical folder that stays. `partial`: the two are only
/// almost the same — what the folder that stays also has goes, the rest stays where it is.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pick {
    pub keep: FolderRef,
    pub copy: FolderRef,
    #[serde(default)]
    pub partial: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FolderRemoveReport {
    pub batch_id: String,
    /// Copy folders that are gone.
    pub folders: u64,
    /// Copy folders emptied of their photos but still there (something else is left inside).
    pub partial: u64,
    /// Not touched: no longer identical, already gone, protected.
    pub skipped: u64,
    pub skipped_why: Vec<String>,
    /// Photos and videos taken out of the copy folders.
    pub files: u64,
    /// Deleted for good (`purged`), else the size waiting in the quarantine.
    pub bytes: i64,
    pub purged: bool,
    /// Compared with the file that stays and found different after all: put back where they
    /// were, and from now on known as different files (to look at side by side).
    pub different: u64,
    /// Files that are not photos or videos, deleted because the folder that stays has the same
    /// file at the same place (and `Thumbs.db` and the like).
    pub other_deleted: u64,
    /// Files left in the copy folders: only there, or not the same as in the folder that stays.
    pub left: u64,
    pub left_examples: Vec<String>,
    /// Photos only the copy folder has (`partial` picks): left where they are.
    pub unique_left: u64,
    /// ♥ and other marks of the user carried over to the file that stays.
    pub labels_moved: u64,
    pub failed: u64,
    pub errors: Vec<String>,
    pub cancelled: bool,
    /// Deleted after the quick check (parts of each file compared) instead of the full one.
    pub quick: bool,
    /// Where the files that turned out different are: pairs of folders to look at side by side.
    pub to_review: Vec<ReviewPair>,
}

/// A copy folder left with files that differ from the original's, and that original folder.
#[derive(Clone, Debug, Serialize)]
pub struct ReviewPair {
    pub keep: FolderRef,
    pub keep_path: String,
    pub copy: FolderRef,
    pub copy_path: String,
    pub files: u64,
}

/// How the copies go: `purge` deletes them once compared with their twins (else they wait in
/// the quarantine); `quick` compares parts of each file instead of all of it.
#[derive(Clone, Copy, Debug, Default)]
pub struct RemoveOptions {
    pub purge: bool,
    pub quick: bool,
}

/// `remove_with`, only for copy folders inside the work scope: a pick of a folder outside is
/// not done (the folder that stays may be anywhere).
pub fn remove_in(db: &Db, picks: &[Pick], opts: &RemoveOptions, scope: Option<&crate::db::Scope>, stop: &StopFlag, progress: ProgressFn) -> Result<FolderRemoveReport> {
    let Some(scope) = scope else { return remove_with(db, picks, opts, stop, progress) };
    let (inside, outside): (Vec<Pick>, Vec<Pick>) = picks.iter().cloned().partition(|p| {
        let c = clean(&p.copy);
        scope.contains_dir(c.volume_id, &c.rel_dir)
    });
    let mut rep = if inside.is_empty() { FolderRemoveReport::default() } else { remove_with(db, &inside, opts, stop, progress)? };
    rep.skipped += outside.len() as u64;
    for p in outside.iter().take(50usize.saturating_sub(rep.skipped_why.len())) {
        rep.skipped_why.push(format!("{}: 작업 범위 밖이라 지우지 않았습니다", p.copy.rel_dir));
    }
    Ok(rep)
}

/// What of `found` concerns the work scope: sets with a copy inside (only those copies are
/// listed; the folder that stays may be anywhere), and almost-the-same folders inside.
pub fn in_scope(mut found: Found, scope: Option<&crate::db::Scope>) -> Found {
    let Some(scope) = scope else { return found };
    for s in &mut found.sets {
        s.copies.retain(|c| scope.contains_dir(c.volume_id, &c.rel_dir));
    }
    found.sets.retain(|s| !s.copies.is_empty());
    for n in &mut found.near {
        n.others.retain(|o| scope.contains_dir(o.info.volume_id, &o.info.rel_dir));
    }
    found.near.retain(|n| !n.others.is_empty());
    found
}

fn clean(r: &FolderRef) -> FolderRef {
    FolderRef { volume_id: r.volume_id, rel_dir: r.rel_dir.replace('\\', "/").trim_matches('/').to_string() }
}

/// Remove the copy folders of `picks`. Each pair is checked again against the index right
/// before (a folder that changed since it was listed is skipped). With `purge` the files are
/// deleted once compared byte for byte with their twin; without it they wait in the quarantine.
pub fn remove(db: &Db, picks: &[Pick], purge: bool, stop: &StopFlag, progress: ProgressFn) -> Result<FolderRemoveReport> {
    remove_with(db, picks, &RemoveOptions { purge, quick: false }, stop, progress)
}

pub fn remove_with(db: &Db, picks: &[Pick], opts: &RemoveOptions, stop: &StopFlag, progress: ProgressFn) -> Result<FolderRemoveReport> {
    let (purge, quick) = (opts.purge, opts.purge && opts.quick);
    let conn = db.conn()?;
    volumes::refresh(&conn)?;
    let vols = load(&conn)?;
    let rules = DupRules::load(&conn)?;
    let lib_vol = library::location(&conn).map(|l| l.0);
    // Outer folders first: a pick inside a folder that goes anyway then finds nothing to do.
    let mut seen: HashSet<(i64, String)> = HashSet::new();
    let mut picks: Vec<Pick> = picks.iter().map(|p| Pick { keep: clean(&p.keep), copy: clean(&p.copy), partial: p.partial }).filter(|p| seen.insert((p.copy.volume_id, p.copy.rel_dir.clone()))).collect();
    picks.sort_by_key(|p| p.copy.rel_dir.matches('/').count());
    let total: u64 = picks.iter().map(|p| vols.iter().find(|v| v.id == p.copy.volume_id).map_or(0, |v| v.subtree(&p.copy.rel_dir).len() as u64)).sum();

    let near = picks.iter().filter(|p| p.partial).count();
    let label = |n: u64| {
        let what = match near {
            0 => format!("100% 복사 폴더 {n}개 정리"),
            m if m == picks.len() => format!("거의 같은 폴더 {n}개에서 겹치는 사진 정리"),
            m => format!("복사 폴더 {n}개 정리 (거의 같은 폴더 {m}개 포함)"),
        };
        if quick { format!("{what} · 빠른 확인으로 지움") } else { what }
    };
    let batch = journal::new_batch(&conn, "copies", &label(picks.len() as u64))?;
    let mut rep = FolderRemoveReport { batch_id: batch.clone(), purged: purge, quick, ..Default::default() };
    let mut run = Run { conn: &conn, vols: &vols, rules: &rules, lib_vol, batch: &batch, purge, stop, progress, gone: HashSet::new(), done: 0, total, emptied: Vec::new() };
    for p in &picks {
        if stop.is_stopped() {
            rep.cancelled = true;
            break;
        }
        match crate::guarded("복사 폴더 정리", || run.one(p, &mut rep)) {
            Ok(None) => {}
            Ok(Some(why)) => {
                rep.skipped += 1;
                if rep.skipped_why.len() < 50 {
                    rep.skipped_why.push(format!("{}: {why}", p.copy.rel_dir));
                }
            }
            Err(e) => {
                rep.failed += 1;
                rep.errors.push(format!("{}: {e}", p.copy.rel_dir));
            }
        }
    }
    let run_emptied = std::mem::take(&mut run.emptied);
    conn.execute("UPDATE batches SET description = ? WHERE id = ?", params![label(rep.folders + rep.partial), batch])?;
    journal::finish_batch(&conn, &batch)?;
    if rep.files == 0 {
        conn.execute("DELETE FROM batches WHERE id = ? AND op_count = 0", [&batch])?;
        rep.batch_id.clear();
    } else if purge && !rep.cancelled {
        let p = journal::purge_batch_opts(&conn, &batch, quick, stop, progress)?;
        rep.bytes = p.bytes;
        rep.failed += p.failed;
        rep.cancelled |= p.cancelled;
        if p.kept > 0 && !p.cancelled {
            // What was not deleted goes back to its place: files that turned out different are
            // the user's to look at, not to sit in a quarantine. The two get fingerprints that
            // tell them apart, so nothing takes them for the same file again.
            let back = journal::undo_batch(&conn, &batch)?;
            conn.execute("UPDATE batches SET state = 'purged' WHERE id = ? AND NOT EXISTS (SELECT 1 FROM journal WHERE batch_id = ?1 AND undone = 0)", [&batch])?;
            rep.files = rep.files.saturating_sub(back.restored);
            rep.failed += back.failed;
            rep.errors.extend(back.errors);
            rep.errors.extend(p.errors.iter().filter(|e| !e.ends_with("남겨 둠")).cloned());
            for (a, b) in &p.differing {
                if !tell_apart(&conn, *a, *b)? {
                    continue;
                }
                rep.different += 1;
                // Which of the picks it belongs to: that pair of folders is worth a look.
                let Some(f) = db::get_file(&conn, *a)? else { continue };
                let Some(pick) = picks.iter().find(|p| p.copy.volume_id == f.volume_id && f.rel_path.starts_with(&format!("{}/", p.copy.rel_dir))) else { continue };
                match rep.to_review.iter_mut().find(|r| r.copy.rel_dir == pick.copy.rel_dir && r.copy.volume_id == pick.copy.volume_id) {
                    Some(r) => r.files += 1,
                    None => {
                        let at = |d: &FolderRef| volumes::join_rel(Path::new(&f.mount), &d.rel_dir).to_string_lossy().into_owned();
                        rep.to_review.push(ReviewPair { keep: pick.keep.clone(), keep_path: at(&pick.keep), copy: pick.copy.clone(), copy_path: at(&pick.copy), files: 1 });
                    }
                }
            }
            for dir in &run_emptied {
                if dir.exists() {
                    rep.folders = rep.folders.saturating_sub(1);
                    rep.partial += 1;
                }
            }
        } else {
            rep.different = p.kept;
            rep.errors.extend(p.errors);
        }
    }
    Ok(rep)
}

struct Run<'a> {
    conn: &'a Connection,
    vols: &'a [Vol],
    rules: &'a DupRules,
    lib_vol: Option<i64>,
    batch: &'a str,
    purge: bool,
    stop: &'a StopFlag,
    progress: ProgressFn<'a>,
    /// Files this run already took out (the loaded index still lists them).
    gone: HashSet<i64>,
    done: u64,
    total: u64,
    /// Copy folders that were gone when their pick was done.
    emptied: Vec<PathBuf>,
}

/// Two files that share a fingerprint (same size, same first 64 KiB) but were found to differ:
/// read both in full and extend their fingerprints by their own hash. `true` if they do differ.
fn tell_apart(conn: &Connection, a: i64, b: i64) -> Result<bool> {
    let hash = |id: i64| -> Option<String> { db::get_file(conn, id).ok().flatten().filter(|f| !f.quarantined && !f.missing).and_then(|f| journal::hash_file(&f.abs_path()).ok()) };
    let (Some(ha), Some(hb)) = (hash(a), hash(b)) else { return Ok(false) };
    if ha == hb {
        return Ok(false);
    }
    for (id, h) in [(a, &ha), (b, &hb)] {
        conn.execute(
            "UPDATE files SET hash = ?1, prehash = CASE WHEN prehash IS NULL THEN NULL
                 ELSE (CASE WHEN instr(prehash, '~') > 0 THEN substr(prehash, 1, instr(prehash, '~') - 1) ELSE prehash END) || '~' || substr(?1, 1, 16) END
             WHERE id = ?2",
            params![h, id],
        )?;
    }
    Ok(true)
}

impl Run<'_> {
    /// `Ok(Some(reason))` when the pair was skipped untouched.
    fn one(&mut self, p: &Pick, rep: &mut FolderRemoveReport) -> Result<Option<String>> {
        let (kd, cd) = (p.keep.rel_dir.as_str(), p.copy.rel_dir.as_str());
        if p.keep.volume_id != p.copy.volume_id {
            return Ok(Some("다른 드라이브의 폴더라서 지우지 않았습니다".into()));
        }
        if self.lib_vol.is_some_and(|l| l != p.copy.volume_id) {
            return Ok(Some("라이브러리 드라이브가 아니라서 건드리지 않았습니다".into()));
        }
        let Some(v) = self.vols.iter().find(|v| v.id == p.copy.volume_id) else {
            return Ok(Some("드라이브가 연결되어 있지 않습니다".into()));
        };
        if kd.is_empty() || cd.is_empty() || kd == cd || nested(kd, cd) {
            return Ok(Some("한 폴더가 다른 폴더 안에 있습니다".into()));
        }
        let (keep_abs, copy_abs) = (v.abs(kd), v.abs(cd));
        if self.rules.class_of_path(&copy_abs.to_string_lossy()) == 0 {
            return Ok(Some("「항상 남길 폴더」라서 지우지 않았습니다".into()));
        }
        let live = |d: &str| -> Vec<&Rec> { v.subtree(d).iter().filter(|r| !self.gone.contains(&r.id)).collect() };
        let (k, c) = (live(kd), live(cd));
        if c.is_empty() {
            // Nothing left: emptied before, or by an outer pick of this very run (not worth a word).
            return Ok(v.subtree(cd).is_empty().then(|| "이미 정리되었습니다".into()));
        }
        if !p.partial {
            if k.iter().chain(c.iter()).any(|r| r.key.is_none()) {
                return Ok(Some("아직 확인하지 못한 파일이 있습니다".into()));
            }
            if content(k.iter().copied()) != content(c.iter().copied()) {
                return Ok(Some("두 폴더의 내용이 달라져서 지우지 않았습니다".into()));
            }
        }
        if !keep_abs.is_dir() {
            return Ok(Some("남길 폴더를 찾을 수 없습니다".into()));
        }
        let twin = twins(&k, kd, &c, cd);
        if twin.is_empty() {
            return Ok(Some("두 폴더에 같은 사진이 없습니다".into()));
        }

        let mut only_here: HashSet<PathBuf> = HashSet::new(); // photos the folder that stays does not have
        let mut stay: HashSet<PathBuf> = HashSet::new(); // indexed files that could not be taken out
        for r in &c {
            let here = v.abs(&r.rel);
            if self.stop.is_stopped() {
                rep.cancelled = true;
                stay.insert(here);
                continue;
            }
            self.done += 1;
            (self.progress)(Progress::new(JOB, "move", self.done, self.total, r.rel.clone()));
            let Some(t) = twin.get(&r.id) else {
                rep.unique_left += 1;
                only_here.insert(here);
                continue;
            };
            let t = Some(t).filter(|t| v.abs(&t.rel).is_file());
            let f = db::get_file(self.conn, r.id)?.filter(|f| !f.quarantined && !f.missing);
            let (Some(t), Some(f)) = (t, f) else {
                stay.insert(here);
                continue;
            };
            let moved =
                self.conn.execute("INSERT OR IGNORE INTO labels(file_id, label, source) SELECT ?2, label, source FROM labels WHERE file_id = ?1 AND source = 'user'", params![r.id, t.id])?;
            rep.labels_moved += moved as u64;
            match journal::quarantine_copy(self.conn, self.batch, &f, t.id) {
                Ok(()) => {
                    self.gone.insert(r.id);
                    rep.files += 1;
                    rep.bytes += f.size;
                }
                Err(e) => {
                    rep.failed += 1;
                    if rep.errors.len() < 200 {
                        rep.errors.push(e.to_string());
                    }
                    stay.insert(here);
                }
            }
        }
        // What else is in the folder (documents, sidecars — nothing the index knows), then the
        // folders that are empty now.
        if !rep.cancelled {
            sweep(&copy_abs, &keep_abs, self.purge, &stay, &only_here, rep);
        }
        if copy_abs.exists() {
            rep.partial += 1;
        } else {
            rep.folders += 1;
            self.emptied.push(copy_abs.clone());
            // Folders that held nothing but the copy (`백업/여행/…`) go with it.
            if let Some(up) = copy_abs.parent() {
                journal::prune_empty_dirs(up, Path::new(&v.mount));
            }
        }
        Ok(None)
    }
}

fn is_junk(lower: &str) -> bool {
    matches!(lower, "thumbs.db" | "desktop.ini" | ".ds_store") || lower.starts_with("._")
}

/// Delete from `dir` what the folder that stays (`twin_dir`) also has at the same place with
/// the same bytes, and the system's own leftovers; then the folder itself if nothing is left.
/// Everything else stays and is counted.
fn sweep(dir: &Path, twin_dir: &Path, purge: bool, stay: &HashSet<PathBuf>, only_here: &HashSet<PathBuf>, rep: &mut FolderRemoveReport) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let (p, name) = (e.path(), e.file_name());
        let kind = e.file_type().ok();
        if kind.is_some_and(|k| k.is_dir()) {
            sweep(&p, &twin_dir.join(&name), purge, stay, only_here, rep);
            continue;
        }
        if only_here.contains(&p) {
            continue; // counted as a photo of its own, not as something left over
        }
        let mut deleted = false;
        if kind.is_some_and(|k| k.is_file()) && !stay.contains(&p) {
            let twin = twin_dir.join(&name);
            let same = purge && twin.is_file() && journal::same_content(&p, &twin).unwrap_or(false);
            deleted = (same || is_junk(&name.to_string_lossy().to_lowercase())) && fs::remove_file(&p).is_ok();
        }
        if deleted {
            rep.other_deleted += 1;
        } else {
            rep.left += 1;
            if rep.left_examples.len() < 20 {
                rep.left_examples.push(p.to_string_lossy().into_owned());
            }
        }
    }
    let _ = fs::remove_dir(dir);
}

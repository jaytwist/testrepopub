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
        "SELECT f.id, f.volume_id, f.rel_path, f.size, COALESCE(f.prehash, f.hash), v.mount
         FROM files f JOIN volumes v ON v.id = f.volume_id
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
        vols[i].files.push(Rec { id: r.get(0)?, rel: r.get(2)?, size: r.get(3)?, key: r.get(4)? });
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

fn fingerprint(key: &str, size: i64) -> u128 {
    let h = blake3::hash(format!("{size}:{key}").as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&h.as_bytes()[..16]);
    u128::from_le_bytes(b)
}

/// A folder's content as a sorted list — what "the same" finally means.
fn content<'a>(files: impl Iterator<Item = &'a Rec>) -> Vec<(&'a str, i64)> {
    let mut l: Vec<(&str, i64)> = files.map(|r| (r.key.as_deref().unwrap_or(""), r.size)).collect();
    l.sort_unstable();
    l
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

#[derive(Clone, Debug, Default, Serialize)]
pub struct Found {
    pub sets: Vec<FolderSet>,
    /// Files not fingerprinted yet that could be copies (another file has their size): folders
    /// holding them are not judged until they are read. (A file whose size is unique is never
    /// read and never a copy.)
    pub unread: u64,
    /// The only drive looked at (the library drive), if one is chosen: its mount path.
    pub drive: Option<String>,
}

/// Order among identical folders (lowest stays): see `info`.
type Rank = (u8, bool, i64, i64, i64);
/// A folder's sorted content, and the folders that have exactly it.
type SameContent<'a> = (Vec<(&'a str, i64)>, Vec<&'a str>);

#[derive(Default, Clone, Copy)]
struct Agg {
    count: u64,
    bytes: i64,
    sum: u128,
    unknown: bool,
}

/// Every set of identical folders, biggest first. Reads the index, and the disk only for the
/// dates of the folders it reports.
pub fn find(conn: &Connection) -> Result<Found> {
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
        find_in(v, &rules, lib.as_ref(), &mut out.sets);
    }
    out.sets.sort_by(|a, b| {
        (b.bytes * b.copies.len() as i64).cmp(&(a.bytes * a.copies.len() as i64)).then_with(|| a.keep.abs_path.cmp(&b.keep.abs_path))
    });
    Ok(out)
}

fn find_in(v: &Vol, rules: &DupRules, lib: Option<&(i64, String)>, out: &mut Vec<FolderSet>) {
    // Each folder's own files, then added up into every folder above it.
    let mut direct: HashMap<&str, Agg> = HashMap::new();
    for r in &v.files {
        let Some(i) = r.rel.rfind('/') else { continue };
        let a = direct.entry(&r.rel[..i]).or_default();
        a.count += 1;
        a.bytes += r.size;
        match &r.key {
            Some(k) => a.sum = a.sum.wrapping_add(fingerprint(k, r.size)),
            None => a.unknown = true,
        }
    }
    let mut total: HashMap<&str, Agg> = HashMap::new();
    for (dir, a) in &direct {
        for d in std::iter::once(*dir).chain(ancestors(dir)) {
            let t = total.entry(d).or_default();
            t.count += a.count;
            t.bytes += a.bytes;
            t.sum = t.sum.wrapping_add(a.sum);
            t.unknown |= a.unknown;
        }
    }
    let mut by_sig: HashMap<(u64, i64, u128), Vec<&str>> = HashMap::new();
    for (d, a) in &total {
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
    let mut covered: HashMap<&str, bool> = HashMap::new(); // folder of a set → it is the one that stays
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
    let first_id = v.subtree(dir).iter().map(|r| r.id).min().unwrap_or(i64::MAX);
    let rank = (class, !filed, created_ms.unwrap_or(i64::MAX), modified_ms.unwrap_or(i64::MAX), first_id);
    Some((
        FolderInfo { volume_id: v.id, rel_dir: dir.to_string(), abs_path: abs_s, created, modified, keep_rule: class == 0, filed, names: names_key(v, dir) },
        rank,
    ))
}

// ---------------------------------------------------------------------------------------------
// Remove
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FolderRef {
    pub volume_id: i64,
    pub rel_dir: String,
}

/// One copy folder to remove, and the identical folder that stays.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pick {
    pub keep: FolderRef,
    pub copy: FolderRef,
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
    /// Left in the quarantine: not byte-identical to the file that stays after all.
    pub different: u64,
    /// Files that are not photos or videos, deleted because the folder that stays has the same
    /// file at the same place (and `Thumbs.db` and the like).
    pub other_deleted: u64,
    /// Files left in the copy folders: only there, or not the same as in the folder that stays.
    pub left: u64,
    pub left_examples: Vec<String>,
    /// ♥ and other marks of the user carried over to the file that stays.
    pub labels_moved: u64,
    pub failed: u64,
    pub errors: Vec<String>,
    pub cancelled: bool,
}

fn clean(r: &FolderRef) -> FolderRef {
    FolderRef { volume_id: r.volume_id, rel_dir: r.rel_dir.replace('\\', "/").trim_matches('/').to_string() }
}

/// Remove the copy folders of `picks`. Each pair is checked again against the index right
/// before (a folder that changed since it was listed is skipped). With `purge` the files are
/// deleted once compared byte for byte with their twin; without it they wait in the quarantine.
pub fn remove(db: &Db, picks: &[Pick], purge: bool, stop: &StopFlag, progress: ProgressFn) -> Result<FolderRemoveReport> {
    let conn = db.conn()?;
    volumes::refresh(&conn)?;
    let vols = load(&conn)?;
    let rules = DupRules::load(&conn)?;
    let lib_vol = library::location(&conn).map(|l| l.0);
    // Outer folders first: a pick inside a folder that goes anyway then finds nothing to do.
    let mut seen: HashSet<(i64, String)> = HashSet::new();
    let mut picks: Vec<Pick> = picks.iter().map(|p| Pick { keep: clean(&p.keep), copy: clean(&p.copy) }).filter(|p| seen.insert((p.copy.volume_id, p.copy.rel_dir.clone()))).collect();
    picks.sort_by_key(|p| p.copy.rel_dir.matches('/').count());
    let total: u64 = picks.iter().map(|p| vols.iter().find(|v| v.id == p.copy.volume_id).map_or(0, |v| v.subtree(&p.copy.rel_dir).len() as u64)).sum();

    let batch = journal::new_batch(&conn, "copies", &format!("100% 복사 폴더 {}개 정리", picks.len()))?;
    let mut rep = FolderRemoveReport { batch_id: batch.clone(), purged: purge, ..Default::default() };
    let mut run = Run { conn: &conn, vols: &vols, rules: &rules, lib_vol, batch: &batch, purge, stop, progress, gone: HashSet::new(), done: 0, total };
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
    conn.execute("UPDATE batches SET description = ? WHERE id = ?", params![format!("100% 복사 폴더 {}개 정리", rep.folders + rep.partial), batch])?;
    journal::finish_batch(&conn, &batch)?;
    if rep.files == 0 {
        conn.execute("DELETE FROM batches WHERE id = ? AND op_count = 0", [&batch])?;
        rep.batch_id.clear();
    } else if purge && !rep.cancelled {
        let p = journal::purge_batch_with(&conn, &batch, stop, progress)?;
        rep.bytes = p.bytes;
        rep.different = p.kept;
        rep.failed += p.failed;
        rep.errors.extend(p.errors);
        rep.cancelled |= p.cancelled;
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
            return Ok(Some("이미 정리되었습니다".into()));
        }
        if k.iter().chain(c.iter()).any(|r| r.key.is_none()) {
            return Ok(Some("아직 확인하지 못한 파일이 있습니다".into()));
        }
        if content(k.iter().copied()) != content(c.iter().copied()) {
            return Ok(Some("두 폴더의 내용이 달라져서 지우지 않았습니다".into()));
        }
        if !keep_abs.is_dir() {
            return Ok(Some("남길 폴더를 찾을 수 없습니다".into()));
        }

        // Each copy's twin in the folder that stays: the file at the same place if it is the
        // same content, else any file with that content (each twin used once).
        let mut pool: HashMap<(&str, i64), Vec<&Rec>> = HashMap::new();
        for r in &k {
            pool.entry((r.key.as_deref().unwrap_or(""), r.size)).or_default().push(r);
        }
        let inner = |r: &Rec, d: &str| r.rel[d.len() + 1..].to_lowercase();
        let mut twin: HashMap<i64, &Rec> = HashMap::new();
        for r in &c {
            let name = inner(r, cd);
            if let Some(l) = pool.get_mut(&(r.key.as_deref().unwrap_or(""), r.size)) {
                if let Some(i) = l.iter().position(|t| inner(t, kd) == name) {
                    twin.insert(r.id, l.swap_remove(i));
                }
            }
        }
        for r in &c {
            if let std::collections::hash_map::Entry::Vacant(slot) = twin.entry(r.id) {
                if let Some(t) = pool.get_mut(&(r.key.as_deref().unwrap_or(""), r.size)).and_then(|l| l.pop()) {
                    slot.insert(t);
                }
            }
        }

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
            let t = twin.get(&r.id).filter(|t| v.abs(&t.rel).is_file());
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
            sweep(&copy_abs, &keep_abs, self.purge, &stay, rep);
        }
        if copy_abs.exists() {
            rep.partial += 1;
        } else {
            rep.folders += 1;
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
fn sweep(dir: &Path, twin_dir: &Path, purge: bool, stay: &HashSet<PathBuf>, rep: &mut FolderRemoveReport) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let (p, name) = (e.path(), e.file_name());
        let kind = e.file_type().ok();
        if kind.is_some_and(|k| k.is_dir()) {
            sweep(&p, &twin_dir.join(&name), purge, stay, rep);
            continue;
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

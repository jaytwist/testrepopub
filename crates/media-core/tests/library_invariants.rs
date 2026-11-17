//! Whatever the order of operations, organizing never loses a photo:
//! random libraries (copies, renamed copies, undated files, photos only on another drive) go
//! through random sequences of plan / push one year (deleting copies or not) / run everything /
//! remove whole-folder copies / undo / rescan. After every step:
//! 1. every content that existed at the start still exists somewhere (copies may go, the last
//!    one of a content never does);
//! 2. the other drive is exactly as it was;
//! 3. every file the index says is there, is there.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use media_core::folders::{self as copy_folders, FolderRef, Pick};
use media_core::journal;
use media_core::library::LibraryOptions;
use media_core::organizer::{self, ExecOptions, OrganizeOptions};
use media_core::protect;
use media_core::rusqlite::params;
use media_core::scanner::{self, ScanOptions};
use media_core::{db, volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

/// Small deterministic generator (no dependency, same sequence every run).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n != ".mo-volume") {
                out.push(p);
            }
        }
    }
    out
}

/// content hash -> how many files hold it (quarantine folders included: not lost yet).
fn contents(roots: &[&Path]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for r in roots {
        for p in files_under(r) {
            *m.entry(journal::hash_file(&p).unwrap()).or_default() += 1;
        }
    }
    m
}

fn snapshot(root: &Path) -> BTreeSet<(String, String)> {
    files_under(root).into_iter().map(|p| (p.strip_prefix(root).unwrap().to_string_lossy().into_owned(), journal::hash_file(&p).unwrap())).collect()
}

fn run(seed: u64) {
    let mut rng = Rng(seed);
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let c = drive(tmp.path(), "diskC");
    let folders = ["옛날/2012-05 제주도", "DCIM/100CANON", "백업/폰", "가족", "옛날/유럽 여행/파리", "Downloads"];
    let mut made: Vec<PathBuf> = Vec::new();
    for i in 0..28u32 {
        let year = 2008 + rng.below(8) as u32;
        let date = format!("{year}:{:02}:{:02} 10:00:00", 1 + rng.below(12), 1 + rng.below(28));
        let on_c = rng.chance(20);
        let root = if on_c { &c } else { &e };
        let p = root.join(folders[rng.below(folders.len())]).join(format!("IMG_{i:03}.jpg"));
        if rng.chance(12) {
            write_png(&p.with_extension("png"), &picture(1000 + i, 640, 480)); // undated
            made.push(p.with_extension("png"));
        } else {
            write_jpeg(&p, &picture(1000 + i, 640, 480), Some((date.as_str(), "Canon", "EOS R6")));
            made.push(p);
        }
    }
    // Copies: same name or renamed, on E or on C.
    for i in 0..made.len() {
        if rng.chance(45) {
            let src = made[i].clone();
            let root = if rng.chance(25) { &c } else { &e };
            let name = if rng.chance(50) { src.file_name().unwrap().to_string_lossy().into_owned() } else { format!("copy {i} of {}", src.file_name().unwrap().to_string_lossy()) };
            let dst = root.join(folders[rng.below(folders.len())]).join(name);
            if !dst.exists() {
                fs::create_dir_all(dst.parent().unwrap()).unwrap();
                fs::copy(&src, &dst).unwrap();
            }
        }
    }
    // Whole folders copied as they are (a file inside renamed now and then).
    for n in 0..2 {
        let name = folders[rng.below(folders.len())];
        let (src, dst) = (e.join(name), e.join(format!("복사/{n}/{} - 복사본", name.rsplit('/').next().unwrap())));
        if src.is_dir() {
            fs::create_dir_all(&dst).unwrap();
            for f in files_under(&src) {
                let to = dst.join(f.strip_prefix(&src).unwrap());
                fs::create_dir_all(to.parent().unwrap()).unwrap();
                let to = if rng.chance(15) { to.with_file_name(format!("r{}", to.file_name().unwrap().to_string_lossy())) } else { to };
                fs::copy(&f, &to).unwrap();
            }
        }
    }
    fs::write(e.join("옛날/유럽 여행/notes.txt"), "not a photo").ok();

    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    volumes::add_source(&conn, &c).unwrap();
    let stop = StopFlag::new();
    let scan = || {
        scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    };
    scan();
    let start = contents(&[&e, &c]);
    let c_before = snapshot(&c);
    let opts = OrganizeOptions {
        target_dir: e.join("사진").to_string_lossy().into_owned(),
        layout: "library".into(),
        library: LibraryOptions::default(),
        ..Default::default()
    };

    // The library location is a saved setting in the app (what tells the drives apart).
    media_core::importer::ImportSettings { target_dir: opts.target_dir.clone(), ..Default::default() }.save(&conn).unwrap();

    // What was done so far (shown when a check fails), and the batches made, in order.
    let log: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
    let mut made_batches: Vec<String> = Vec::new();
    let check = |step: &str| {
        log.borrow_mut().push(step.to_string());
        let log = log.borrow().join("\n  ");
        let now = contents(&[&e, &c]);
        for k in start.keys() {
            assert!(now.contains_key(k), "seed {seed}: a photo is gone (content {k}) after:\n  {log}");
        }
        assert_eq!(snapshot(&c), c_before, "seed {seed}: the other drive changed after:\n  {log}");
        let mut st = conn.prepare("SELECT v.mount, f.rel_path FROM files f JOIN volumes v ON v.id = f.volume_id WHERE f.missing = 0 AND f.quarantined = 0").unwrap();
        for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).unwrap() {
            let (mount, rel) = r.unwrap();
            let p = volumes::join_rel(Path::new(&mount), &rel);
            assert!(p.is_file(), "seed {seed}: indexed but not on disk: {} after:\n  {log}", p.display());
        }
    };

    for round in 0..10 {
        match rng.below(7) {
            // Push one year that has work, deleting its copies for good or not.
            0 | 1 => {
                let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
                let years: Vec<_> = organizer::year_buckets(&plan).into_iter().filter(|y| !y.done).collect();
                if let Some(y) = years.get(rng.below(years.len().max(1))) {
                    let xo = ExecOptions { purge: rng.chance(60), label: Some(y.label.clone()), ..Default::default() };
                    let rep = organizer::execute_with(&db, &organizer::subset_year(&plan, &y.key), &xo, &stop, &quiet).unwrap();
                    assert_eq!(rep.failed, 0, "seed {seed} round {round} push {}: {:?} after:\n  {}", y.key, rep.errors, log.borrow().join("\n  "));
                    made_batches.extend([rep.copies_batch.clone(), rep.batch_id.clone()].into_iter().filter(|b| !b.is_empty()));
                    check(&format!("round {round}: push {} (purge {})", y.key, xo.purge));
                }
            }
            // Everything at once.
            2 => {
                let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
                let rep = organizer::execute(&db, &plan, &stop, &quiet).unwrap();
                assert_eq!(rep.failed, 0, "seed {seed} round {round} run all: {:?} after:\n  {}", rep.errors, log.borrow().join("\n  "));
                made_batches.extend([rep.copies_batch.clone(), rep.batch_id.clone()].into_iter().filter(|b| !b.is_empty()));
                check(&format!("round {round}: run all"));
            }
            // Undo something still undoable (may partly fail if files moved on since: nothing
            // may be lost either way).
            3 => {
                let state: HashMap<String, (String, String)> =
                    journal::list_batches(&conn).unwrap().into_iter().map(|b| (b.id.clone(), (b.state.clone(), b.description.clone()))).collect();
                let undoable: Vec<&String> = made_batches.iter().filter(|b| state.get(*b).is_some_and(|s| s.0 == "done")).collect();
                if !undoable.is_empty() {
                    let b = undoable[rng.below(undoable.len())];
                    let r = journal::undo_batch(&conn, b);
                    check(&format!("round {round}: undo {} -> {:?}", state[b].1, r.map(|u| (u.restored, u.failed))));
                }
            }
            // Remove whole-folder copies: the suggested ones, or keeping a copy instead.
            4 => {
                let found = copy_folders::find(&conn).unwrap();
                let r = |f: &copy_folders::FolderInfo| FolderRef { volume_id: f.volume_id, rel_dir: f.rel_dir.clone() };
                let mut picks: Vec<Pick> = Vec::new();
                for s in &found.sets {
                    if rng.chance(30) {
                        // The first copy stays; the suggested folder and the other copies go.
                        picks.push(Pick { keep: r(&s.copies[0]), copy: r(&s.keep), partial: false });
                        picks.extend(s.copies[1..].iter().map(|c| Pick { keep: r(&s.copies[0]), copy: r(c), partial: false }));
                    } else {
                        picks.extend(s.copies.iter().map(|c| Pick { keep: r(&s.keep), copy: r(c), partial: false }));
                    }
                }
                // And "what both have goes" between any two folders: safe whatever they hold.
                if rng.chance(50) {
                    let vol: Option<i64> = found.sets.first().map(|s| s.keep.volume_id).or_else(|| conn.query_row("SELECT volume_id FROM sources ORDER BY id LIMIT 1", [], |r| r.get(0)).ok());
                    let (x, y) = (folders[rng.below(folders.len())], folders[rng.below(folders.len())]);
                    if let Some(volume_id) = vol.filter(|_| x != y) {
                        picks.push(Pick { keep: FolderRef { volume_id, rel_dir: x.into() }, copy: FolderRef { volume_id, rel_dir: y.into() }, partial: true });
                    }
                }
                picks.extend(found.near.iter().flat_map(|n| n.others.iter().map(|o| Pick { keep: r(&n.base), copy: r(&o.info), partial: true })));
                let purge = rng.chance(60);
                let rep = copy_folders::remove(&db, &picks, purge, &stop, &quiet).unwrap();
                assert_eq!(rep.failed, 0, "seed {seed} round {round} copy folders: {:?} after:\n  {}", rep.errors, log.borrow().join("\n  "));
                made_batches.extend(Some(rep.batch_id.clone()).filter(|b| !b.is_empty()));
                check(&format!("round {round}: remove {} copy folders (purge {purge}) -> {} files, {} skipped", picks.len(), rep.files, rep.skipped));
            }
            // Take files of one folder into another: what only the second has comes along, and
            // a file under the same name with other content replaces the first's (which waits in
            // the quarantine).
            5 => {
                let vol: Option<i64> = conn.query_row("SELECT volume_id FROM sources ORDER BY id LIMIT 1", [], |r| r.get(0)).ok();
                let (x, y) = (folders[rng.below(folders.len())], folders[rng.below(folders.len())]);
                if let Some(volume_id) = vol.filter(|_| x != y) {
                    let (keep, copy) = (FolderRef { volume_id, rel_dir: x.into() }, FolderRef { volume_id, rel_dir: y.into() });
                    let rows = copy_folders::align(&conn, &keep, std::slice::from_ref(&copy)).unwrap();
                    let choices: Vec<copy_folders::Choice> = rows
                        .iter()
                        .filter(|r| r.kind != "same")
                        .filter_map(|r| Some(copy_folders::Choice { keep_file: r.files[0].as_ref().map(|f| f.id), copy_file: r.files[1].as_ref()?.id, discard: false }))
                        .collect();
                    let rep = copy_folders::swap(&db, &keep, &[copy_folders::SwapGroup { copy, choices }], &stop, &quiet).unwrap();
                    assert_eq!(rep.failed, 0, "seed {seed} round {round} swap: {:?} after:\n  {}", rep.errors, log.borrow().join("\n  "));
                    made_batches.extend(Some(rep.batch_id.clone()).filter(|b| !b.is_empty()));
                    check(&format!("round {round}: {y} into {x} -> {} replaced, {} brought", rep.replaced, rep.brought));
                }
            }
            _ => {
                scan();
                check(&format!("round {round}: rescan"));
            }
        }
    }

    // In the end, with everything run: nothing left to do, and each content is in the library
    // exactly once (or only on the other drive's copy was needed — it was copied in).
    scan();
    let plan = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    organizer::execute_with(&db, &plan, &ExecOptions { purge: true, ..Default::default() }, &stop, &quiet).unwrap();
    check("final run");
    let again = organizer::plan(&db, &opts, &stop, &quiet).unwrap();
    let todo: Vec<String> = again.items.iter().filter(|i| !matches!(i.action, organizer::Action::InPlace | organizer::Action::Leave)).map(|i| format!("{:?} {} -> {} [{} / {}]", i.action, i.src, i.dst, i.tag, i.group)).collect();
    assert_eq!((again.summary.moves, again.summary.copies, again.summary.remove), (0, 0, 0), "seed {seed}: still to do {todo:#?} after:\n  {}", log.borrow().join("\n  "));
    let lib = contents(&[&e.join("사진")]);
    let not_media = journal::hash_file(&e.join("옛날/유럽 여행/notes.txt")).ok();
    for k in start.keys().filter(|k| Some(*k) != not_media.as_ref()) {
        assert_eq!(lib.get(k).copied().unwrap_or(0), 1, "seed {seed}: each photo is in the library once (content {k}) after:\n  {}", log.borrow().join("\n  "));
    }
}

#[test]
fn nothing_is_ever_lost() {
    // `MO_INVARIANT_SEEDS=200` tries that many more sequences (by hand; the default is quick).
    let extra: u64 = std::env::var("MO_INVARIANT_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let only: Option<u64> = std::env::var("MO_INVARIANT_ONLY").ok().and_then(|v| v.parse().ok());
    // (138: pushes and undos that once left a photo filed one folder too deep.)
    for seed in [1, 7, 42, 138, 2024, 99991].into_iter().chain(100..100 + extra).filter(|s| only.is_none_or(|o| o == *s)) {
        run(seed);
    }
}

/// The bulk protection check (used for a whole plan's copies) answers exactly like the per-file
/// one.
#[test]
fn bulk_protection_check_matches() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let vid: i64 = conn.query_row("SELECT volume_id FROM sources", [], |r| r.get(0)).unwrap();
    let mut rng = Rng(5);
    for i in 0..900 {
        // Contents shared by 1–4 files; a few without a fingerprint, missing or quarantined.
        let key = if rng.chance(5) { None } else { Some(format!("k{}", rng.below(350))) };
        conn.execute(
            "INSERT INTO files(volume_id, rel_path, name, ext, kind, size, mtime, prehash, missing, quarantined)
             VALUES (?1, ?2, ?3, 'jpg', 'photo', 10, 0, ?4, ?5, ?6)",
            params![vid, format!("d{}/f{i}.jpg", rng.below(20)), format!("f{i}.jpg"), key, rng.chance(3) as i64, rng.chance(3) as i64],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        if rng.chance(6) {
            conn.execute("INSERT INTO labels(file_id, label, source) VALUES (?, 'favorite', 'user')", [id]).unwrap();
        }
        if rng.chance(4) {
            conn.execute("INSERT OR IGNORE INTO labels(file_id, label, source) VALUES (?, 'original', 'user')", [id]).unwrap();
        }
        if rng.chance(10) {
            conn.execute("INSERT INTO dup_roles(file_id, role, group_size) VALUES (?, ?, 2)", params![id, if rng.chance(50) { "original" } else { "copy" }]).unwrap();
        }
    }
    let rows = db::all_files(&conn).unwrap();
    let all: Vec<i64> = (1..=900).collect();
    for take in [900usize, 700, 520] {
        let ids: Vec<i64> = all.iter().copied().filter(|_| rng.chance(80)).take(take).collect();
        assert!(ids.len() >= 500, "the bulk path needs at least 500 files");
        let a: HashMap<i64, String> = protect::check(&conn, &ids).unwrap().into_iter().map(|b| (b.id, b.reason)).collect();
        let b: HashMap<i64, String> = protect::check_many(&conn, &ids, &rows).unwrap().into_iter().map(|b| (b.id, b.reason)).collect();
        assert_eq!(a, b);
        assert!(a.values().any(|r| r == "last_copy") && a.values().any(|r| r == "favorite"), "the data exercises the rules");
    }
}

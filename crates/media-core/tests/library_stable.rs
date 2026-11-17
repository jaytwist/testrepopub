//! 「한 곳으로 정리」 lands every photo in the same place whatever the way there: everything at
//! once, one year at a time (in any order), or with a push undone in between — and a library
//! that is done has nothing left to move when planned again.
//!
//! What once went wrong: `옛날` holds photos of many years, so it is a container, not an event.
//! With some years already pushed, the photos left in it spanned a few days only; `옛날` then
//! looked like an event and the rest was filed as `월/옛날/제주도` instead of `월/제주도` — and the
//! next plan wanted to move those once more.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use media_core::journal;
use media_core::library::LibraryOptions;
use media_core::organizer::{self, ExecOptions, OrganizeOptions, Plan};
use media_core::scanner::{self, ScanOptions};
use media_core::{volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

/// One drive with the same photos every time, indexed, and the options to organize it.
struct World {
    _tmp: tempfile::TempDir,
    e: PathBuf,
    db: Db,
    opts: OrganizeOptions,
    stop: StopFlag,
}

const PHOTOS: &[(&str, &str)] = &[
    // A container of many years: events inside, a trip with days, and loose photos.
    ("옛날/제주도/j1.jpg", "2015:03:10 10:00:00"),
    ("옛날/제주도/j2.jpg", "2015:03:12 10:00:00"),
    ("옛날/2012-05 제주도/k1.jpg", "2012:05:03 10:00:00"),
    ("옛날/2012-05 제주도/k2.jpg", "2012:05:04 10:00:00"),
    ("옛날/유럽 여행/파리/p1.jpg", "2013:07:02 10:00:00"),
    ("옛날/유럽 여행/런던/l1.jpg", "2013:07:09 10:00:00"),
    ("옛날/loose14.jpg", "2014:01:20 10:00:00"),
    // A folder whose only sub-folder holds the photos, over two years.
    ("모음/행사/돌잔치/d1.jpg", "2014:06:01 10:00:00"),
    ("모음/행사/돌잔치/d2.jpg", "2014:06:01 11:00:00"),
    ("모음/행사/졸업식/g1.jpg", "2016:02:20 10:00:00"),
    // Camera folders and a family folder across the years.
    ("DCIM/100CANON/c12.jpg", "2012:09:01 10:00:00"),
    ("DCIM/100CANON/c15.jpg", "2015:03:20 10:00:00"),
    ("DCIM/100CANON/c16.jpg", "2016:11:05 10:00:00"),
    ("가족/f13.jpg", "2013:12:25 10:00:00"),
    ("가족/f15.jpg", "2015:10:01 10:00:00"),
];

fn world(extra: &[(&str, &str)]) -> World {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    for (i, (rel, date)) in PHOTOS.iter().chain(extra).enumerate() {
        write_jpeg(&e.join(rel), &picture(500 + i as u32, 640, 480), Some((date, "Canon", "EOS R6")));
    }
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    let stop = StopFlag::new();
    scanner::scan_all(&db, &ScanOptions::default(), &stop, &quiet).unwrap();
    let opts = OrganizeOptions { target_dir: e.join("사진").to_string_lossy().into_owned(), layout: "library".into(), library: LibraryOptions::default(), ..Default::default() };
    World { _tmp: tmp, e, db, opts, stop }
}

impl World {
    fn plan(&self) -> Plan {
        organizer::plan(&self.db, &self.opts, &self.stop, &quiet).unwrap()
    }

    /// Push one year; the batch made.
    fn push(&self, key: &str) -> String {
        let plan = self.plan();
        let xo = ExecOptions { label: Some(key.into()), ..Default::default() };
        let rep = organizer::execute_with(&self.db, &organizer::subset_year(&plan, key), &xo, &self.stop, &quiet).unwrap();
        assert_eq!(rep.failed, 0, "push {key}: {:?}", rep.errors);
        rep.batch_id
    }

    /// The years that still have work, in the order shown.
    fn years(&self) -> Vec<String> {
        organizer::year_buckets(&self.plan()).into_iter().filter(|y| !y.done).map(|y| y.key).collect()
    }

    fn run_all(&self) {
        let rep = organizer::execute(&self.db, &self.plan(), &self.stop, &quiet).unwrap();
        assert_eq!(rep.failed, 0, "{:?}", rep.errors);
    }

    /// Every file of the library, by its place under `사진`.
    fn library(&self) -> BTreeSet<String> {
        let root = self.e.join("사진");
        let mut out = BTreeSet::new();
        let mut stack = vec![root.clone()];
        while let Some(d) = stack.pop() {
            for x in fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = x.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.insert(p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"));
                }
            }
        }
        out
    }

    /// A finished library has nothing left to do.
    fn settled(&self, what: &str) {
        let again = self.plan();
        let todo: Vec<String> = again.items.iter().filter(|i| i.src != i.dst).map(|i| format!("{} -> {}", rel(&self.e, &i.src), rel(&self.e, &i.dst))).collect();
        assert_eq!((again.summary.moves, again.summary.copies, again.summary.remove), (0, 0, 0), "{what}: still to do {todo:#?}");
    }
}

fn rel(root: &Path, p: &str) -> String {
    Path::new(p).strip_prefix(root).map(|x| x.to_string_lossy().replace('\\', "/")).unwrap_or_else(|_| p.to_string())
}

#[test]
fn the_same_library_whatever_the_order() {
    let all = world(&[]);
    all.run_all();
    let want = all.library();
    assert_eq!(want.len(), PHOTOS.len(), "{want:#?}");
    // The container is not a folder of the library; its events are.
    assert!(want.contains("2015/2015-03/제주도/j1.jpg"), "{want:#?}");
    assert!(want.contains("2012/2012-05/제주도/k1.jpg"), "{want:#?}");
    assert!(!want.iter().any(|p| p.contains("/옛날/") || p.contains("/모음/")), "{want:#?}");
    all.settled("all at once");

    // One year at a time: oldest first, newest first, and from the middle outwards.
    type Pick = fn(&[String]) -> usize;
    let orders: [(&str, Pick); 3] = [("oldest first", |_| 0), ("newest first", |y| y.len() - 1), ("middle first", |y| y.len() / 2)];
    for (name, pick) in orders {
        let w = world(&[]);
        let mut rounds = 0;
        loop {
            let years = w.years();
            if years.is_empty() {
                break;
            }
            w.push(&years[pick(&years)]);
            rounds += 1;
            assert!(rounds <= 10, "{name}: the years never end: {years:?}");
        }
        assert!(rounds >= 5, "{name}: one push per year ({rounds})");
        assert_eq!(w.library(), want, "{name}");
        w.settled(name);
    }
}

#[test]
fn a_push_undone_changes_nothing() {
    let all = world(&[]);
    all.run_all();
    let want = all.library();

    let w = world(&[]);
    let b15 = w.push("2015");
    w.push("2013");
    // 2015 goes back to where it was; `옛날` holds 2012, 2014 and 2015 again.
    let conn = w.db.conn().unwrap();
    let undo = journal::undo_batch(&conn, &b15).unwrap();
    assert_eq!(undo.failed, 0, "{undo:?}");
    assert!(w.e.join("옛날/제주도/j1.jpg").is_file());
    w.push("2012");
    w.push("2014");
    // Only one spring is left in `옛날` now: it is still not an event.
    w.run_all();
    assert_eq!(w.library(), want);
    w.settled("push, undo, push, run all");
}

#[test]
fn what_is_filed_stays_in_its_folder() {
    // A library made by an earlier build: one folder deeper than it would be made today, and a
    // folder the user made inside a month.
    let w = world(&[
        ("사진/2015/2015-03/옛날/제주도/old1.jpg", "2015:03:11 10:00:00"),
        ("사진/2015/2015-03/옛날/제주도/old2.jpg", "2015:03:11 11:00:00"),
        ("사진/2014/2014-06/우리집/모임/m1.jpg", "2014:06:15 10:00:00"),
    ]);
    let plan = w.plan();
    let moved: Vec<String> = plan.items.iter().filter(|i| i.src != i.dst && rel(&w.e, &i.src).starts_with("사진/")).map(|i| format!("{} -> {}", rel(&w.e, &i.src), rel(&w.e, &i.dst))).collect();
    assert!(moved.is_empty(), "filed photos are not moved again: {moved:#?}");
    w.run_all();
    let lib = w.library();
    for kept in ["2015/2015-03/옛날/제주도/old1.jpg", "2015/2015-03/옛날/제주도/old2.jpg", "2014/2014-06/우리집/모임/m1.jpg"] {
        assert!(lib.contains(kept), "{kept}: {lib:#?}");
    }
    // What comes in new is filed as always.
    assert!(lib.contains("2015/2015-03/제주도/j1.jpg"), "{lib:#?}");
    w.settled("a library with deeper folders");
}

//! Work the screen asks for must stay bounded however fast the user moves: a few thumbnails at
//! a time with the newest first, comparisons that give up when no longer wanted, and lists that
//! are not computed again while nothing changed.

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use media_core::folders::{self, FoundCache};
use media_core::scanner::{self, ScanOptions};
use media_core::{volumes, Db, Progress, StopFlag, WorkStack};

mod common;
use common::*;

fn quiet(_: Progress) {}

#[test]
fn only_so_many_at_once_and_the_newest_first() {
    // Three workers, sixty jobs: never more than three run together, and all are done.
    let stack = WorkStack::new("t", 3);
    let (running, most, done) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (tx, rx) = mpsc::channel();
    for _ in 0..60 {
        let (running, most, done, tx) = (running.clone(), most.clone(), done.clone(), tx.clone());
        stack.push(move || {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(3));
            running.fetch_sub(1, Ordering::SeqCst);
            done.fetch_add(1, Ordering::SeqCst);
            let _ = tx.send(());
        });
    }
    for _ in 0..60 {
        rx.recv_timeout(Duration::from_secs(10)).expect("every job is done");
    }
    assert_eq!(done.load(Ordering::SeqCst), 60);
    assert!(most.load(Ordering::SeqCst) <= 3, "{} ran together", most.load(Ordering::SeqCst));
    assert_eq!(stack.queued(), 0);

    // One worker held busy while five jobs arrive: they are taken newest first.
    let stack = WorkStack::new("t", 1);
    let (hold_tx, hold_rx) = mpsc::channel::<()>();
    let (started_tx, started_rx) = mpsc::channel();
    stack.push(move || {
        let _ = started_tx.send(());
        let _ = hold_rx.recv();
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let order: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    for n in 1..=5 {
        let (order, tx) = (order.clone(), tx.clone());
        stack.push(move || {
            order.lock().unwrap().push(n);
            let _ = tx.send(());
        });
    }
    assert_eq!(stack.queued(), 5);
    hold_tx.send(()).unwrap();
    for _ in 0..5 {
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    assert_eq!(*order.lock().unwrap(), [5, 4, 3, 2, 1]);

    // A job that panics does not take its worker with it.
    let (tx, rx) = mpsc::channel();
    stack.push(|| panic!("a broken photo"));
    stack.push(move || {
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(5)).expect("the worker is still there");
}

#[test]
fn a_comparison_nobody_waits_for_gives_up_and_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let e = drive(tmp.path(), "diskE");
    // Two files of 8 MiB, the same in their first 64 KiB (so: one fingerprint), different at the end.
    let mut data = vec![7u8; 8 << 20];
    fs::create_dir_all(e.join("a")).unwrap();
    fs::create_dir_all(e.join("b")).unwrap();
    fs::write(e.join("a/v.jpg"), &data).unwrap();
    *data.last_mut().unwrap() = 9;
    fs::write(e.join("b/v.jpg"), &data).unwrap();
    let db = Db::open(tmp.path().join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    scanner::scan_all(&db, &ScanOptions { min_image_bytes: 0, min_image_side: 0, ..Default::default() }, &StopFlag::new(), &quiet).unwrap();
    let id = |rel: &str| -> i64 { conn.query_row("SELECT id FROM files WHERE rel_path = ?", [rel], |r| r.get(0)).unwrap() };
    let fp = |rel: &str| -> String { conn.query_row("SELECT prehash FROM files WHERE rel_path = ?", [rel], |r| r.get(0)).unwrap() };
    let before = (fp("a/v.jpg"), fp("b/v.jpg"));
    assert_eq!(before.0, before.1);

    // Given up after the third megabyte: an error, long before the end, and the index untouched.
    let asked = AtomicUsize::new(0);
    let r = folders::check_pair_with(&conn, id("a/v.jpg"), id("b/v.jpg"), &|| asked.fetch_add(1, Ordering::SeqCst) >= 3);
    assert_eq!(r.unwrap_err().to_string(), "cancelled");
    assert_eq!(asked.load(Ordering::SeqCst), 4, "stopped at once, not read to the end");
    assert_eq!((fp("a/v.jpg"), fp("b/v.jpg")), before);
    // Let run, it finds the difference at the last byte and tells the two apart.
    let d = folders::check_pair(&conn, id("a/v.jpg"), id("b/v.jpg")).unwrap();
    assert_eq!(d.at, Some((8 << 20) - 1));
    assert_ne!(fp("a/v.jpg"), fp("b/v.jpg"));
}

#[test]
fn a_list_is_computed_once_until_something_changes() {
    let cache = FoundCache::default();
    let runs = AtomicUsize::new(0);
    let compute = || {
        runs.fetch_add(1, Ordering::SeqCst);
        Ok(folders::Found { unread: runs.load(Ordering::SeqCst) as u64, ..Default::default() })
    };
    // The same generation and bar: from memory.
    assert_eq!(cache.get(1, 0.95, compute).unwrap().unread, 1);
    assert_eq!(cache.get(1, 0.95, compute).unwrap().unread, 1);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    // Another bar, then files changed: computed again each time.
    assert_eq!(cache.get(1, 0.80, compute).unwrap().unread, 2);
    assert_eq!(cache.get(2, 0.80, compute).unwrap().unread, 3);
    assert_eq!(cache.get(2, 0.80, compute).unwrap().unread, 3);
    assert_eq!(runs.load(Ordering::SeqCst), 3);
    // A computation that fails is not remembered.
    assert!(cache.get(3, 0.80, || anyhow::bail!("disk gone")).is_err());
    assert_eq!(cache.get(3, 0.80, compute).unwrap().unread, 4);

    // Two callers at once: one computes, the other takes its result.
    let cache = Arc::new(FoundCache::default());
    let runs = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (cache, runs) = (cache.clone(), runs.clone());
            std::thread::spawn(move || {
                cache
                    .get(7, 0.95, || {
                        runs.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(50));
                        Ok(folders::Found::default())
                    })
                    .unwrap();
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

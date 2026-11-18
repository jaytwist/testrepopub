//! Deleting copies after comparing them with their twins: the comparison itself (whole files in
//! long runs, or the quick check on parts), what the progress says, and going on after a stop.

use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::thread::sleep;
use std::time::Duration;

use media_core::folders::{self, FolderRef, Pick, RemoveOptions};
use media_core::journal::{self, sample_blocks, Comparer};
use media_core::scanner::{self, ScanOptions};
use media_core::{volumes, Db, Progress, StopFlag};

mod common;
use common::*;

fn quiet(_: Progress) {}

fn bytes(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

#[test]
fn whole_files_are_compared_across_run_boundaries() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    // Runs of 1000 bytes: sizes around one and several runs.
    let mut cmp = Comparer::with_run(1000);
    for n in [0usize, 1, 999, 1000, 1001, 2000, 2500] {
        let data = bytes(n, 7);
        fs::write(&a, &data).unwrap();
        fs::write(&b, &data).unwrap();
        assert!(cmp.same(&a, &b).unwrap(), "{n} bytes, identical");
        // One byte different: the first, one in each run, the last.
        for at in [0, n / 2, n.saturating_sub(1), 999.min(n.saturating_sub(1)), 1000.min(n.saturating_sub(1))] {
            if n == 0 {
                continue;
            }
            let mut other = data.clone();
            other[at] ^= 0xFF;
            fs::write(&b, &other).unwrap();
            assert!(!cmp.same(&a, &b).unwrap(), "{n} bytes, byte {at} differs");
        }
        // One byte shorter.
        if n > 0 {
            fs::write(&b, &data[..n - 1]).unwrap();
            assert!(!cmp.same(&a, &b).unwrap(), "{n} bytes against {} bytes", n - 1);
        }
    }
    // The default run length on a small file, and the plain function.
    fs::write(&a, bytes(5000, 1)).unwrap();
    fs::write(&b, bytes(5000, 1)).unwrap();
    assert!(Comparer::new().same(&a, &b).unwrap() && journal::same_content(&a, &b).unwrap());
}

#[test]
fn the_quick_check_reads_parts_and_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a.mp4"), tmp.path().join("b.mp4"));
    let len = 40 << 20; // 40 MiB: two stripes
    let data = bytes(len, 3);
    fs::write(&a, &data).unwrap();
    let blocks = sample_blocks(len as u64);
    // First and last megabyte, and one megabyte in each 32 MiB stripe: 4 of 40 MiB.
    assert_eq!(blocks.first().map(|b| b.0), Some(0));
    assert_eq!(blocks.last().map(|b| b.0 + b.1 as u64), Some(len as u64));
    assert_eq!(blocks.len(), 4, "{blocks:?}");
    assert_eq!(sample_blocks(len as u64), blocks, "the same places for a file and its copy");
    let inside = |at: usize| blocks.iter().any(|(o, l)| (*o as usize..*o as usize + l).contains(&at));
    let mut cmp = Comparer::new();
    let mut differs_at = |at: usize| {
        let mut other = data.clone();
        other[at] ^= 0xFF;
        fs::write(&b, &other).unwrap();
        (cmp.same_sampled(&a, &b).unwrap(), cmp.same(&a, &b).unwrap())
    };
    fs::write(&b, &data).unwrap();
    assert!(Comparer::new().same_sampled(&a, &b).unwrap());
    // Seen: a difference at the very start, at the very end, inside a sampled block.
    assert_eq!(differs_at(0), (false, false));
    assert_eq!(differs_at(len - 1), (false, false));
    let mid = blocks[1].0 as usize + 500;
    assert_eq!(differs_at(mid), (false, false));
    // Not seen: a difference between the blocks. The full comparison sees it — this is what
    // the quick check gives up, and why it is an option that is off.
    let gap = (blocks[0].1..len).find(|at| !inside(*at)).unwrap();
    assert_eq!(differs_at(gap), (true, false), "byte {gap} is in no sampled block");
    // A different size is always seen; small files are compared whole.
    fs::write(&b, &data[..len - 1]).unwrap();
    assert!(!Comparer::new().same_sampled(&a, &b).unwrap());
    let small = bytes(3 << 20, 9);
    fs::write(&a, &small).unwrap();
    let mut other = small.clone();
    other[1_500_000] ^= 1;
    fs::write(&b, &other).unwrap();
    assert!(!Comparer::new().same_sampled(&a, &b).unwrap(), "up to 4 MiB every byte counts");
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap().flatten() {
        fs::copy(e.path(), to.join(e.file_name())).unwrap();
    }
}

/// A drive with a folder of `n` files and its copy, indexed; the pick that removes the copy.
fn pair(tmp: &Path, n: u32) -> (Db, std::path::PathBuf, Vec<Pick>) {
    let e = drive(tmp, "diskE");
    for i in 0..n {
        fs::create_dir_all(e.join("원본")).unwrap();
        fs::write(e.join(format!("원본/f{i:02}.jpg")), bytes(50_000 + i as usize * 1000, i as u8)).unwrap();
    }
    sleep(Duration::from_millis(30));
    copy_tree(&e.join("원본"), &e.join("복사"));
    let db = Db::open(tmp.join("app/library.db")).unwrap();
    let conn = db.conn().unwrap();
    volumes::add_source(&conn, &e).unwrap();
    scanner::scan_all(&db, &ScanOptions { min_image_bytes: 0, min_image_side: 0, ..Default::default() }, &StopFlag::new(), &quiet).unwrap();
    let vol: i64 = conn.query_row("SELECT volume_id FROM sources LIMIT 1", [], |r| r.get(0)).unwrap();
    let r = |d: &str| FolderRef { volume_id: vol, rel_dir: d.into() };
    (db, e, vec![Pick { keep: r("원본"), copy: r("복사"), partial: false }])
}

#[test]
fn progress_counts_bytes_and_a_stopped_run_goes_on_later() {
    let tmp = tempfile::tempdir().unwrap();
    let (db, e, picks) = pair(tmp.path(), 12);
    let conn = db.conn().unwrap();
    let all: i64 = (0..12).map(|i| 50_000 + i * 1000).sum();

    // Stop after the fourth file was compared.
    let stop = StopFlag::new();
    let seen: Mutex<Vec<Progress>> = Mutex::new(Vec::new());
    let watch = |p: Progress| {
        if p.stage == "verify" {
            if p.done >= 4 {
                stop.stop();
            }
            seen.lock().unwrap().push(p);
        }
    };
    let rep = folders::remove_with(&db, &picks, &RemoveOptions { purge: true, quick: false }, &stop, &watch).unwrap();
    assert!(rep.cancelled, "{rep:?}");
    assert_eq!(rep.files, 12, "all were taken out of the folder");
    let seen = seen.into_inner().unwrap();
    assert!(seen.iter().all(|p| p.bytes_total == Some(all as u64)), "the whole size is known from the start");
    let done: Vec<u64> = seen.iter().filter_map(|p| p.bytes_done).collect();
    assert!(done.windows(2).all(|w| w[0] <= w[1]) && done[0] == 0 && *done.last().unwrap() < all as u64, "{done:?}");
    let waiting = |e: &Path| walk(e).into_iter().filter(|p| p.contains(".media-organizer-quarantine")).count();
    assert!(rep.bytes > 0 && rep.bytes < all, "part of it is deleted: {rep:?}");
    let left = waiting(&e);
    assert!(left > 0 && left < 12, "{left} wait in the quarantine");

    // The history shows how many still wait (that is what its "go on deleting" button is for).
    let row = |id: &str| journal::list_batches(&conn).unwrap().into_iter().find(|b| b.id == id).unwrap();
    assert_eq!((row(&rep.batch_id).waiting as usize, row(&rep.batch_id).state.as_str()), (left, "done"));
    // Later, from the history: the rest goes, with the same checks.
    let last: Mutex<Option<Progress>> = Mutex::new(None);
    let r = journal::purge_batch_progress(&conn, &rep.batch_id, false, &StopFlag::new(), &|p| *last.lock().unwrap() = Some(p)).unwrap();
    assert_eq!((r.restored as usize, r.failed), (left, 0), "{r:?}");
    assert_eq!(waiting(&e), 0);
    let last = last.into_inner().unwrap().unwrap();
    assert_eq!((last.done, last.total, last.bytes_done), (left as u64, left as u64, last.bytes_total));
    assert_eq!(walk(&e.join("원본")).len(), 12, "the originals are all there");
    let state: String = conn.query_row("SELECT state FROM batches WHERE id = ?", [&rep.batch_id], |r| r.get(0)).unwrap();
    assert_eq!(state, "purged");
    assert_eq!(row(&rep.batch_id).waiting, 0);
}

#[test]
fn the_quick_check_still_keeps_what_differs_where_it_looks() {
    let tmp = tempfile::tempdir().unwrap();
    let (db, e, picks) = pair(tmp.path(), 3);
    // After indexing, the copy of f01 changes in its last byte (same size, same fingerprint).
    let p = e.join("복사/f01.jpg");
    let mut data = fs::read(&p).unwrap();
    *data.last_mut().unwrap() ^= 0xFF;
    fs::write(&p, &data).unwrap();
    let rep = folders::remove_with(&db, &picks, &RemoveOptions { purge: true, quick: true }, &StopFlag::new(), &quiet).unwrap();
    assert!(rep.quick);
    assert_eq!((rep.files, rep.different), (2, 1), "{rep:?}");
    assert!(!walk(&e).iter().any(|p| p.contains(&rep.batch_id)), "nothing waits in the quarantine");
    assert_eq!(walk(&e.join("복사")).len(), 1, "the one that differs is back in its place");
    assert_eq!(fs::read(e.join("복사/f01.jpg")).unwrap(), data);
    let conn = db.conn().unwrap();
    let what: String = conn.query_row("SELECT description FROM batches WHERE id = ?", [&rep.batch_id], |r| r.get(0)).unwrap();
    assert_eq!(what, "100% 복사 폴더 1개 정리 · 빠른 확인으로 지움");
    // Without deleting (quarantine only) there is nothing quick about it.
    let rep = folders::remove_with(&db, &[], &RemoveOptions { purge: false, quick: true }, &StopFlag::new(), &quiet).unwrap();
    assert!(!rep.quick);
}

fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out
}

/// Throughput of the three ways to compare, on this machine (by hand):
/// `MO_BENCH_MB=1024 cargo test --release -p media-core --test verified_delete -- --ignored --nocapture`
#[test]
#[ignore]
fn compare_speed() {
    use std::io::Read;
    use std::time::Instant;
    let mb: usize = std::env::var("MO_BENCH_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(512);
    let tmp = tempfile::tempdir_in(std::env::var("MO_BENCH_DIR").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned())).unwrap();
    // Half the bytes in 16 MiB "photos", half in 128 MiB "videos".
    let mut files: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    let mut written = 0usize;
    let mut i = 0u8;
    while written < mb << 20 {
        let n = if written < (mb << 20) / 2 { 16 << 20 } else { 128 << 20 };
        let data = bytes(n, i);
        let (a, b) = (tmp.path().join(format!("a{i}")), tmp.path().join(format!("b{i}")));
        fs::write(&a, &data).unwrap();
        fs::write(&b, &data).unwrap();
        files.push((a, b));
        written += n;
        i += 1;
    }
    let total = written as f64 / (1 << 20) as f64;
    let report = |name: &str, t: Instant| println!("{name:<28} {:>8.0} MB/s  ({:.2?} for {total:.0} MB in {} files)", total / t.elapsed().as_secs_f64(), t.elapsed(), files.len());
    // Before: one megabyte of each file in turns.
    let t = Instant::now();
    for (a, b) in &files {
        let (mut fa, mut fb) = (fs::File::open(a).unwrap(), fs::File::open(b).unwrap());
        let (mut ba, mut bb) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
        loop {
            let n = fa.read(&mut ba).unwrap();
            let m = fb.read(&mut bb).unwrap();
            assert!(n == m && ba[..n] == bb[..m]);
            if n == 0 {
                break;
            }
        }
    }
    report("1 MiB in turns (before)", t);
    for run_mb in [4usize, 8, 16, 32, 64] {
        let mut cmp = Comparer::with_run(run_mb << 20);
        let t = Instant::now();
        for (a, b) in &files {
            assert!(cmp.same(a, b).unwrap());
        }
        report(&format!("{run_mb} MiB runs"), t);
    }
    let mut cmp = Comparer::new();
    let t = Instant::now();
    for (a, b) in &files {
        assert!(cmp.same_sampled(a, b).unwrap());
    }
    report("quick check (parts)", t);
}

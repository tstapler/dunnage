#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.3.3 Task 6.3.3b: `RealDirLister` over real directories in a tempdir. The memory bound is measured
//! with `peak_alloc` (a safe counting allocator); the tests of this binary take turns so none disturbs it.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use disk_cleanup_adapters::RealDirLister;
use disk_cleanup_core::explore::msg::FileRows;
use disk_cleanup_core::explore::ports::DirLister;
use peak_alloc::PeakAlloc;

#[global_allocator]
static ALLOC: PeakAlloc = PeakAlloc;

/// The allocator counts every thread, so the tests of this binary take turns.
static TURN: Mutex<()> = Mutex::new(());

fn turn() -> MutexGuard<'static, ()> {
    TURN.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The most bytes `f` held at once beyond what was live when it started.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = ALLOC.current_usage();
    ALLOC.reset_peak_usage();
    let out = f();
    let peak = ALLOC.peak_usage().saturating_sub(before);
    (out, peak)
}

fn never() -> AtomicBool {
    AtomicBool::new(false)
}

/// `n` files named `f00000`..; file `i` is `((i % 50) + 1) * 4096` bytes of non-zero data.
fn many_files(dir: &Path, n: usize) {
    let block = vec![7u8; 50 * 4096];
    for i in 0..n {
        let len = ((i % 50) + 1) * 4096;
        fs::File::create(dir.join(format!("f{i:05}")))
            .unwrap()
            .write_all(&block[..len])
            .unwrap();
    }
}

/// What a correct listing must say, computed independently with `lstat` over the directory.
fn oracle(dir: &Path, top_n: usize) -> (Vec<(String, u64, u64)>, u64, u64) {
    let mut all: Vec<(String, u64, u64)> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap())
        .filter_map(|e| {
            let m = fs::symlink_metadata(e.path()).unwrap();
            (!m.is_dir()).then(|| {
                (
                    e.file_name().into_string().unwrap(),
                    m.blocks() * 512,
                    m.len(),
                )
            })
        })
        .collect();
    all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let rest = all.split_off(top_n.min(all.len()));
    (all, rest.len() as u64, rest.iter().map(|r| r.1).sum())
}

fn triples(rows: &FileRows) -> Vec<(String, u64, u64)> {
    rows.rows
        .iter()
        .map(|r| {
            (
                r.name.to_str().unwrap().to_string(),
                r.allocated_bytes,
                r.size_bytes,
            )
        })
        .collect()
}

#[test]
fn the_5000_largest_of_20000_files_should_come_back_with_the_totals_of_the_rest_in_bounded_memory()
{
    let d = tempfile::tempdir().unwrap();
    many_files(d.path(), 20_000);
    let (want, want_count, want_bytes) = oracle(d.path(), 5_000);
    assert_eq!((want.len(), want_count), (5_000, 15_000));

    let lister = RealDirLister;
    let cancel = never();
    let (rows, peak) = peak_of(|| lister.list(d.path(), 5_000, &cancel).unwrap());
    assert_eq!(
        triples(&rows),
        want,
        "the same 5,000, largest first, ties by name"
    );
    assert_eq!(rows.smaller_count, 15_000);
    assert_eq!(rows.smaller_bytes, want_bytes);
    assert!(!rows.partial);
    assert!(rows.rows.iter().all(|r| !r.second_link));
    let held = rows.rows.capacity() * std::mem::size_of_val(&rows.rows[0]);
    eprintln!("peak {peak} bytes for top_n 5000 over 20000 files (result vector {held})");
    // top_n entries at ~(name + heap node) each, with room for read_dir's buffer; the 15,000 others add nothing
    assert!(peak < 5_000 * 160 + 256 * 1024, "{peak}");

    // the bound is by top_n, not by the directory: the same 20,000 files with top_n 100 hold far less
    let (small, peak_small) = peak_of(|| lister.list(d.path(), 100, &cancel).unwrap());
    assert_eq!(small.rows.len(), 100);
    assert_eq!(small.smaller_count, 19_900);
    assert!(peak_small < 100 * 160 + 128 * 1024, "{peak_small}");
    assert!(peak_small * 4 < peak, "{peak_small} vs {peak}");

    cancel_mid_listing_stops_within_100ms(d.path());
}

/// Sets the flag while a listing of the big directory is running; tries again if a fast machine finished
/// first, and requires that at least one attempt caught it mid-way.
fn cancel_mid_listing_stops_within_100ms(dir: &Path) {
    let mut caught = false;
    for attempt in 0..50u64 {
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let path = dir.to_path_buf();
        let worker = std::thread::spawn(move || RealDirLister.list(&path, 10, &flag).unwrap());
        std::thread::sleep(Duration::from_micros(200 + 300 * attempt));
        let set_at = Instant::now();
        cancel.store(true, Ordering::SeqCst);
        let rows = worker.join().unwrap();
        if rows.partial {
            let lag = set_at.elapsed();
            assert!(
                lag < Duration::from_millis(100),
                "stopped {lag:?} after the cancel"
            );
            assert!(rows.rows.len() <= 10);
            caught = true;
            break;
        }
    }
    assert!(caught, "no attempt caught the listing mid-way");

    // a flag set before the call returns at once with nothing gathered
    let flag = AtomicBool::new(true);
    let started = Instant::now();
    let r = RealDirLister.list(dir, 10, &flag).unwrap();
    assert!(r.partial && r.rows.is_empty());
    assert!(started.elapsed() < Duration::from_millis(100));
}

#[test]
fn directories_should_be_skipped_symlinks_listed_with_their_own_size_and_odd_names_kept_raw() {
    let _turn = turn();
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    fs::create_dir(p.join("subdir")).unwrap();
    fs::write(p.join("real.bin"), vec![1u8; 40_000]).unwrap();
    symlink("real.bin", p.join("link-to-big")).unwrap();
    symlink("/nonexistent/target/of/some/length", p.join("dangling")).unwrap();
    fs::write(p.join("line\nbreak"), b"x").unwrap();
    let raw = OsString::from_vec(b"bad-\xff\xfe-utf8".to_vec());
    fs::write(p.join(&raw), b"yy").unwrap();

    let rows = RealDirLister.list(p, 100, &never()).unwrap();
    let names: Vec<&OsString> = rows.rows.iter().map(|r| &r.name).collect();
    assert!(!names.iter().any(|n| *n == "subdir"));
    assert_eq!(rows.rows.len(), 5);
    let by_name = |n: &str| rows.rows.iter().find(|r| r.name == n).unwrap();
    assert_eq!(by_name("real.bin").size_bytes, 40_000);
    assert_eq!(
        by_name("link-to-big").size_bytes,
        "real.bin".len() as u64,
        "the link's own length, not its target's"
    );
    assert_eq!(
        by_name("dangling").size_bytes,
        "/nonexistent/target/of/some/length".len() as u64
    );
    assert!(rows.rows.iter().any(|r| r.name == "line\nbreak"));
    assert!(
        rows.rows.iter().any(|r| r.name == raw),
        "invalid UTF-8 comes back as the raw OsString"
    );
    assert_eq!(rows.rows[0].name, "real.bin", "largest first");
    assert_eq!((rows.smaller_count, rows.smaller_bytes), (0, 0));
}

#[test]
fn a_second_hard_link_should_be_marked_within_one_listing() {
    let _turn = turn();
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    fs::write(p.join("a"), vec![1u8; 8192]).unwrap();
    fs::hard_link(p.join("a"), p.join("b")).unwrap();
    fs::write(p.join("solo"), vec![1u8; 4096]).unwrap();
    let rows = RealDirLister.list(p, 10, &never()).unwrap();
    let marked: Vec<&OsString> = rows
        .rows
        .iter()
        .filter(|r| r.second_link)
        .map(|r| &r.name)
        .collect();
    assert_eq!(
        marked.len(),
        1,
        "exactly one of the two links is the second"
    );
    assert!(marked[0] == "a" || marked[0] == "b");
    assert!(!rows.rows.iter().any(|r| r.name == "solo" && r.second_link));
}

#[test]
fn an_unreadable_directory_should_return_the_errno_and_the_port_should_say_partial() {
    let _turn = turn();
    let d = tempfile::tempdir().unwrap();
    let locked = d.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("f"), b"x").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let readable = fs::read_dir(&locked).is_ok(); // running as root reads it anyway
    let got = RealDirLister.list(&locked, 10, &never());
    let via_port = RealDirLister.list_files(&locked, 10, &never());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        eprintln!("SKIP: this user reads mode 000 directories");
        return;
    }
    assert_eq!(got.unwrap_err(), rustix::io::Errno::ACCESS.raw_os_error());
    assert!(via_port.partial && via_port.rows.is_empty());

    let missing = d.path().join("nope");
    assert_eq!(
        RealDirLister.list(&missing, 10, &never()).unwrap_err(),
        rustix::io::Errno::NOENT.raw_os_error()
    );
}

#[test]
fn listing_100000_entries_should_be_timed_and_reported() {
    let _turn = turn();
    let d = tempfile::tempdir().unwrap();
    for i in 0..100_000u32 {
        fs::File::create(d.path().join(format!("e{i:06}"))).unwrap();
    }
    let started = Instant::now();
    let rows = RealDirLister.list(d.path(), 5_000, &never()).unwrap();
    let took = started.elapsed();
    eprintln!("RealDirLister over 100,000 entries, top_n 5000: {took:?}");
    assert_eq!(rows.rows.len(), 5_000);
    assert_eq!(rows.smaller_count, 95_000);
}

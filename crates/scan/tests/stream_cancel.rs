#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.1.2: cancel is prompt and keeps the work, the save is gated, the end of every walk happens in one
//! pinned order, and cancel wakes idle workers.

mod stream_common;

use disk_cleanup_scan::cache;
use disk_cleanup_scan::index::{CollectRequest, DirEvent, StreamRequest};
use disk_cleanup_scan::walker::{CacheConfig, WalkerBackend};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use stream_common::{FakeFs, Recorder, FAKE_MTIME, ROOT};

fn tmp(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "dc-scan-cancel-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn request<'a>(
    roots: &'a [PathBuf],
    threads: usize,
    cancel: &Arc<AtomicBool>,
) -> StreamRequest<'a> {
    StreamRequest::new(
        CollectRequest {
            roots,
            cross_fs: false,
            deadline: None,
            threads,
        },
        Arc::clone(cancel),
    )
}

fn cached(dir: &Path) -> WalkerBackend {
    WalkerBackend {
        cache: Some(CacheConfig {
            path: dir.join("c.bin"),
            rescan: false,
            max_age: Duration::from_secs(3600),
        }),
    }
}

fn cancel_after(cancel: &Arc<AtomicBool>, d: Duration) -> std::thread::JoinHandle<()> {
    let c = Arc::clone(cancel);
    std::thread::spawn(move || {
        std::thread::sleep(d);
        c.store(true, Ordering::Relaxed);
    })
}

#[test]
fn cancel_should_return_within_the_bound_truncated_and_save_what_was_visited_When_walk_of_200000_dirs(
) {
    let fake = FakeFs::new(200_000, 10);
    let dir = tmp("prompt");
    let roots = vec![PathBuf::from(ROOT)];
    let cancel = Arc::new(AtomicBool::new(false));
    let timer = cancel_after(&cancel, Duration::from_millis(50));
    let rec = Recorder::default();
    let t = Instant::now();
    let summary = cached(&dir)
        .stream(&request(&roots, 8, &cancel), &rec.sink(), &fake)
        .unwrap();
    let took = t.elapsed();
    timer.join().unwrap();
    assert!(summary.truncated);
    assert!(took < Duration::from_secs(3), "returned in {took:?}");
    let visited = fake.finished.load(Ordering::Relaxed);
    assert!(visited < 200_000, "the walk was cut short");
    let saved = cache::load(&dir.join("c.bin"));
    assert_eq!(
        saved.len() as u64,
        visited,
        "the cache holds what was visited"
    );
    let delivered: HashSet<String> = rec
        .events()
        .iter()
        .filter_map(|e| match e {
            DirEvent::Dir { name, .. } => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    assert!(
        saved.keys().all(|p| p.as_os_str() == ROOT
            || delivered.contains(p.file_name().unwrap().to_str().unwrap())),
        "no saved directory was left undelivered"
    );
    assert_eq!(
        rec.terminal_count() as u64,
        visited,
        "terminal events at the sink equal the directories the workers finished"
    );
}

#[test]
fn walk_should_return_at_once_with_no_events_When_the_token_is_set_before_the_start() {
    let fake = FakeFs::new(1000, 4);
    let roots = vec![PathBuf::from(ROOT)];
    let cancel = Arc::new(AtomicBool::new(true));
    let rec = Recorder::default();
    let summary = WalkerBackend::default()
        .stream(&request(&roots, 4, &cancel), &rec.sink(), &fake)
        .unwrap();
    assert!(summary.truncated);
    assert_eq!(rec.terminal_count(), 0);
    assert_eq!(fake.finished.load(Ordering::Relaxed), 0);
}

#[test]
fn walk_should_write_no_cache_or_temp_file_When_the_save_gate_is_clear() {
    let fake = FakeFs::new(100_000, 10);
    let dir = tmp("gate");
    let roots = vec![PathBuf::from(ROOT)];
    let cancel = Arc::new(AtomicBool::new(false));
    let req = request(&roots, 4, &cancel);
    req.save.store(false, Ordering::Relaxed);
    let timer = cancel_after(&cancel, Duration::from_millis(30));
    let summary = cached(&dir)
        .stream(&req, &Recorder::default().sink(), &fake)
        .unwrap();
    timer.join().unwrap();
    assert!(summary.truncated);
    assert_eq!(
        fs::read_dir(&dir).unwrap().count(),
        0,
        "neither the cache nor a temp file exists"
    );
}

#[test]
fn truncated_save_should_keep_the_old_records_of_unvisited_directories_When_100_of_2000_were_visited(
) {
    let mut fake = FakeFs::new(2000, 8);
    {
        let mut c = fake.cache.lock().unwrap();
        for n in 0..2000 {
            // stale mtime and a marker inode: the live walk re-lists these directories
            let mut r = fake.rec(n, FAKE_MTIME - 5, 1);
            r.ino = 7;
            c.insert(fake.path_of(n), r);
        }
    }
    let dir = tmp("trunc");
    let roots = vec![PathBuf::from(ROOT)];
    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = Arc::clone(&cancel);
    fake.on_finish = Box::new(move |_, n| {
        if n == 100 {
            c2.store(true, Ordering::Relaxed);
        }
    });
    let summary = cached(&dir)
        .stream(
            &request(&roots, 1, &cancel),
            &Recorder::default().sink(),
            &fake,
        )
        .unwrap();
    assert!(summary.truncated);
    let saved = cache::load(&dir.join("c.bin"));
    assert_eq!(saved.len(), 2000);
    let fresh = saved.values().filter(|r| r.ino != 7).count();
    assert_eq!(
        fresh, 100,
        "the 100 visited records replaced their old ones; the other 1,900 stayed"
    );
}

#[test]
fn save_should_abort_and_leave_no_file_When_the_gate_is_cleared_during_the_write() {
    let fake = FakeFs::new(1, 1);
    let mut old = std::collections::HashMap::new();
    for n in 0..400_000u64 {
        old.insert(
            PathBuf::from(format!("/old/{n}")),
            fake.rec(0, FAKE_MTIME, 1),
        );
    }
    *fake.cache.lock().unwrap() = old;
    let dir = tmp("abort");
    let roots = vec![PathBuf::from(ROOT)];
    let cancel = Arc::new(AtomicBool::new(false));
    let req = request(&roots, 1, &cancel);
    let gate = Arc::clone(&req.save);
    let watch_dir = dir.clone();
    let clearer = std::thread::spawn(move || {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(20) {
            let writing = fs::read_dir(&watch_dir)
                .unwrap()
                .any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".tmp"));
            if writing {
                gate.store(false, Ordering::Relaxed);
                return;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    });
    let summary = cached(&dir)
        .stream(&req, &Recorder::default().sink(), &fake)
        .unwrap();
    clearer.join().unwrap();
    assert!(
        summary.notes.iter().any(|n| n.contains("aborted")),
        "notes: {:?}",
        summary.notes
    );
    assert!(!dir.join("c.bin").exists());
    assert_eq!(
        fs::read_dir(&dir).unwrap().count(),
        0,
        "no temp file is left behind"
    );
}

#[test]
fn cancel_should_wake_idle_workers_within_150ms_When_one_worker_holds_the_last_directory() {
    let mut fake = FakeFs::new(2, 1);
    fake.delay = Box::new(|n| {
        if n == 1 {
            Duration::from_millis(500)
        } else {
            Duration::ZERO
        }
    });
    let roots = vec![PathBuf::from(ROOT)];
    let cancel = Arc::new(AtomicBool::new(false));
    let fake = Arc::new(fake);
    let walk = {
        let (fake, cancel, roots) = (Arc::clone(&fake), Arc::clone(&cancel), roots.clone());
        std::thread::spawn(move || {
            WalkerBackend::default()
                .stream(
                    &request(&roots, 8, &cancel),
                    &Recorder::default().sink(),
                    &*fake,
                )
                .unwrap();
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    cancel.store(true, Ordering::Relaxed);
    let t = Instant::now();
    while fake.exited.load(Ordering::Relaxed) < 7 && t.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(2));
    }
    let woke = t.elapsed();
    assert!(
        fake.exited.load(Ordering::Relaxed) >= 7,
        "idle workers never left"
    );
    assert!(
        woke < Duration::from_millis(150),
        "the 7 idle workers took {woke:?} after the token"
    );
    walk.join().unwrap();
}

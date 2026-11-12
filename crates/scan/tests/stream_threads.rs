#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.1.2: every walk (finished, cancelled before or during, failing lists) leaves no thread behind.
//! Its own test binary, so `/proc/self/task` is not disturbed by other tests' threads.

mod stream_common;

use dunnage_scan::cache::Rec;
use dunnage_scan::index::{CollectRequest, StreamRequest};
use dunnage_scan::walker::{DirStat, WalkHooks, WalkerBackend};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use stream_common::{thread_count, FakeFs, Recorder, FAKE_MTIME, ROOT};

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

/// `list` fails on every directory.
struct ListFails;

impl WalkHooks for ListFails {
    fn stat_override(&self, _p: &Path) -> Option<Result<DirStat, i32>> {
        Some(Ok(FakeFs::stat(0, FAKE_MTIME)))
    }
    fn list_override(&self, _p: &Path, _s: &DirStat) -> Option<Result<Rec, i32>> {
        Some(Err(13))
    }
}

/// A scoped thread reports completion slightly before the kernel removes its task, so give exits 500 ms to land;
/// a thread that is still running stays in the count.
fn settle(before: usize) {
    let t = Instant::now();
    while thread_count() != before && t.elapsed() < Duration::from_millis(500) {
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn walks_should_leave_no_threads_behind_and_a_tiny_walk_should_return_promptly() {
    let roots = vec![PathBuf::from(ROOT)];
    let before = thread_count();
    let mut worst_tiny = Duration::ZERO;
    for i in 0..100 {
        let cancel = Arc::new(AtomicBool::new(i % 5 == 1));
        let rec = Recorder::default();
        let backend = WalkerBackend::default();
        match i % 5 {
            0 => {
                backend
                    .stream(&request(&roots, 4, &cancel), &rec.sink(), &ListFails)
                    .unwrap();
            }
            2 => {
                // cancelled mid-walk
                let c = Arc::clone(&cancel);
                let mut fake = FakeFs::new(300, 3);
                fake.on_finish = Box::new(move |_, n| {
                    if n == 20 {
                        c.store(true, Ordering::Relaxed);
                    }
                });
                backend
                    .stream(&request(&roots, 4, &cancel), &rec.sink(), &fake)
                    .unwrap();
            }
            3 => {
                let fake = FakeFs::new(3, 3);
                let t = Instant::now();
                backend
                    .stream(&request(&roots, 4, &cancel), &rec.sink(), &fake)
                    .unwrap();
                worst_tiny = worst_tiny.max(t.elapsed());
            }
            _ => {
                // finished (4) and cancelled before the start (1)
                let fake = FakeFs::new(300, 3);
                backend
                    .stream(&request(&roots, 4, &cancel), &rec.sink(), &fake)
                    .unwrap();
            }
        }
        settle(before);
        assert_eq!(thread_count(), before, "walk {i} left a thread behind");
    }
    assert!(
        worst_tiny < Duration::from_millis(100),
        "a 3-directory walk took {worst_tiny:?}"
    );
}

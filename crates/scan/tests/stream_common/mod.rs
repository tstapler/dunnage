#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! Shared helpers of the `stream_*` tests: a recording sink and a virtual filesystem driven through
//! `WalkHooks`, so trees of 200,000 or 1,000,000 directories need no disk.

use dunnage_scan::cache::Rec;
use dunnage_scan::estimate::Stats;
use dunnage_scan::index::DirEvent;
use dunnage_scan::walker::{DirStat, WalkHooks};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A k-ary heap of `total` directories: node `n` is named `d{n}` (the root, `/fake`, is node 0) and has the
/// children `n*k+1 ..= n*k+k` that are below `total`.
pub struct Heap {
    pub total: u64,
    pub k: u64,
}

pub const ROOT: &str = "/fake";

pub fn node_of(path: &Path) -> Option<u64> {
    if path == Path::new(ROOT) {
        return Some(0);
    }
    path.parent().filter(|p| p.starts_with(ROOT))?;
    path.file_name()?.to_str()?.strip_prefix('d')?.parse().ok()
}

impl Heap {
    pub fn kids(&self, n: u64) -> u64 {
        let first = n * self.k + 1;
        self.total.saturating_sub(first).min(self.k)
    }
}

/// A fake filesystem plus counters and optional per-directory delays.
pub struct FakeFs {
    pub heap: Heap,
    pub delay: Box<dyn Fn(u64) -> Duration + Send + Sync>,
    pub finished: AtomicU64,
    pub exited: AtomicU64,
    pub loads: AtomicU64,
    pub on_finish: Box<dyn Fn(u64, u64) + Send + Sync>,
    pub cache: Mutex<HashMap<PathBuf, Rec>>,
}

impl FakeFs {
    pub fn new(total: u64, k: u64) -> Self {
        Self {
            heap: Heap { total, k },
            delay: Box::new(|_| Duration::ZERO),
            finished: AtomicU64::new(0),
            exited: AtomicU64::new(0),
            loads: AtomicU64::new(0),
            on_finish: Box::new(|_, _| {}),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn path_of(&self, n: u64) -> PathBuf {
        if n == 0 {
            PathBuf::from(ROOT)
        } else {
            let mut chain = vec![n];
            let mut cur = n;
            while cur != 0 {
                cur = (cur - 1) / self.heap.k;
                chain.push(cur);
            }
            let mut p = PathBuf::from(ROOT);
            for c in chain.iter().rev().skip(1) {
                p.push(format!("d{c}"));
            }
            p
        }
    }

    pub fn stat(n: u64, mtime: i64) -> DirStat {
        DirStat {
            dev: 1,
            ino: n + 100,
            mtime_sec: mtime,
            mtime_nsec: 0,
            size: 4096,
            blocks: 8,
            is_dir: true,
        }
    }

    pub fn rec(&self, n: u64, mtime: i64, listed_at: u64) -> Rec {
        Rec {
            ino: n + 100,
            mtime_sec: mtime,
            mtime_nsec: 0,
            listed_at,
            files: Stats::default(),
            has_package_json: false,
            has_cargo_toml: false,
            git_file: false,
            subdirs: (0..self.heap.kids(n))
                .map(|i| format!("d{}", n * self.heap.k + 1 + i).into_bytes())
                .collect(),
        }
    }
}

pub const FAKE_MTIME: i64 = 1_000_000;

impl WalkHooks for FakeFs {
    fn load_cache(&self, _path: &Path) -> HashMap<PathBuf, Rec> {
        self.loads.fetch_add(1, Ordering::Relaxed);
        self.cache.lock().unwrap().clone()
    }

    fn stat_override(&self, path: &Path) -> Option<Result<DirStat, i32>> {
        let n = node_of(path)?;
        Some(if n < self.heap.total {
            Ok(Self::stat(n, FAKE_MTIME))
        } else {
            Err(libc_enoent())
        })
    }

    fn list_override(&self, path: &Path, _st: &DirStat) -> Option<Result<Rec, i32>> {
        let n = node_of(path)?;
        let d = (self.delay)(n);
        if !d.is_zero() {
            std::thread::sleep(d);
        }
        Some(Ok(self.rec(n, FAKE_MTIME, 1)))
    }

    fn dir_finished(&self, id: u64) {
        let n = self.finished.fetch_add(1, Ordering::Relaxed) + 1;
        (self.on_finish)(id, n);
    }

    fn worker_exited(&self) {
        self.exited.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn libc_enoent() -> i32 {
    2
}

/// Terminal-event arrival record of a sink.
#[derive(Default)]
pub struct Recorder {
    pub batches: Mutex<Vec<(Instant, Vec<DirEvent>)>>,
}

impl Recorder {
    pub fn sink(&self) -> impl Fn(Vec<DirEvent>) + Sync + '_ {
        move |events| self.batches.lock().unwrap().push((Instant::now(), events))
    }

    pub fn events(&self) -> Vec<DirEvent> {
        self.batches
            .lock()
            .unwrap()
            .iter()
            .flat_map(|(_, e)| e.iter().cloned())
            .collect()
    }

    pub fn terminal_count(&self) -> usize {
        self.batches
            .lock()
            .unwrap()
            .iter()
            .map(|(_, e)| e.len())
            .sum()
    }

    pub fn max_batch(&self) -> usize {
        self.batches
            .lock()
            .unwrap()
            .iter()
            .map(|(_, e)| e.len())
            .max()
            .unwrap_or(0)
    }
}

pub fn thread_count() -> usize {
    if !cfg!(target_os = "linux") {
        // macOS has no /proc: `ps -M` prints one line per thread, after one header line
        let out = std::process::Command::new("ps")
            .args(["-M", "-p", &std::process::id().to_string()])
            .output()
            .unwrap();
        return String::from_utf8_lossy(&out.stdout)
            .lines()
            .count()
            .saturating_sub(1);
    }
    std::fs::read_dir("/proc/self/task").unwrap().count()
}

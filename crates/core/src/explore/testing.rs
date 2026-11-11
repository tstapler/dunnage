//! Fakes of the explorer's ports (Task 6.2.1b), behind `test-support`. They are scripted and deterministic.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};

use super::msg::{FileRows, PlanPreview, ScanMsg};

use super::ports::{
    DirLister, ExplorerActions, Handoff, PreviewHandle, ScanHandle, ScanOpts, ScanSource,
    TermSignals,
};
/// The walker's record types, so a crate that builds `NodeEvent`s in tests (the `tui` crate, which may not
/// depend on the scan crate) names them here.
pub use disk_cleanup_scan::estimate::Stats;
pub use disk_cleanup_scan::index::DirInfo;

/// The channel depth of the real source; a consumer that stops receiving blocks the sender.
pub const FAKE_CHANNEL_DEPTH: usize = 64;

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Replays a scripted list of messages from its own thread over a bounded channel.
#[derive(Debug, Default)]
pub struct FakeScanSource {
    script: Mutex<Vec<ScanMsg>>,
}

impl FakeScanSource {
    pub fn new(script: Vec<ScanMsg>) -> FakeScanSource {
        FakeScanSource {
            script: Mutex::new(script),
        }
    }

    /// Shuffles the order of the batches (not their contents) with a seeded generator; `Finished` stays
    /// last, so the reducer still sees every event before the end.
    #[must_use]
    pub fn permuted(self, seed: u64) -> FakeScanSource {
        {
            let mut s = self
                .script
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let at = s
                .iter()
                .position(|m| matches!(m, ScanMsg::Finished(_)))
                .unwrap_or(s.len());
            let mut state = seed | 1;
            for i in (1..at).rev() {
                let j = (xorshift(&mut state) % (i as u64 + 1)) as usize;
                s.swap(i, j);
            }
        }
        self
    }

    /// Interleaves the batches of two scripts one for one (a paint and a live pass in flight together).
    pub fn interleaved(a: Vec<ScanMsg>, b: Vec<ScanMsg>) -> FakeScanSource {
        let mut out = Vec::with_capacity(a.len() + b.len());
        let (mut a, mut b) = (a.into_iter(), b.into_iter());
        loop {
            match (a.next(), b.next()) {
                (None, None) => break,
                (x, y) => out.extend(x.into_iter().chain(y)),
            }
        }
        FakeScanSource::new(out)
    }
}

impl ScanSource for FakeScanSource {
    fn start(&self, _roots: &[PathBuf], _opts: &ScanOpts) -> ScanHandle {
        let script = self
            .script
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let (tx, rx) = sync_channel(FAKE_CHANNEL_DEPTH);
        let cancel = Arc::new(AtomicBool::new(false));
        let save = Arc::new(AtomicBool::new(true));
        let stop = Arc::clone(&cancel);
        std::thread::spawn(move || {
            for m in script {
                if stop.load(Ordering::SeqCst) || tx.send(m).is_err() {
                    return;
                }
            }
        });
        ScanHandle::new(rx, cancel, save)
    }
}

/// Reports a signal once it has been asked `at_tick` times.
#[derive(Debug)]
pub struct FakeTermSignals {
    signal: i32,
    at_tick: usize,
    ticks: AtomicUsize,
    suspend: AtomicBool,
    suspended: AtomicUsize,
}

impl FakeTermSignals {
    pub fn never() -> FakeTermSignals {
        FakeTermSignals::at_tick(usize::MAX, 0)
    }

    pub fn at_tick(at_tick: usize, signal: i32) -> FakeTermSignals {
        FakeTermSignals {
            signal,
            at_tick,
            ticks: AtomicUsize::new(0),
            suspend: AtomicBool::new(false),
            suspended: AtomicUsize::new(0),
        }
    }

    pub fn request_suspend(&self) {
        self.suspend.store(true, Ordering::SeqCst);
    }

    pub fn suspend_calls(&self) -> usize {
        self.suspended.load(Ordering::SeqCst)
    }
}

impl TermSignals for FakeTermSignals {
    fn interrupted(&self) -> Option<i32> {
        let t = self.ticks.fetch_add(1, Ordering::SeqCst) + 1;
        (t >= self.at_tick).then_some(self.signal)
    }

    fn suspend_requested(&self) -> bool {
        self.suspend.swap(false, Ordering::SeqCst)
    }

    fn suspend(&self) {
        self.suspended.fetch_add(1, Ordering::SeqCst);
    }
}

/// Answers every listing with one scripted result.
#[derive(Debug, Default)]
pub struct FakeDirLister {
    pub rows: FileRows,
    calls: Mutex<Vec<PathBuf>>,
}

impl FakeDirLister {
    pub fn new(rows: FileRows) -> FakeDirLister {
        FakeDirLister {
            rows,
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<PathBuf> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl DirLister for FakeDirLister {
    fn list_files(&self, dir: &Path, _top_n: usize, _cancel: &AtomicBool) -> FileRows {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(dir.to_path_buf());
        self.rows.clone()
    }
}

/// Answers a preview with one scripted plan and records what was asked.
#[derive(Debug)]
pub struct FakeExplorerActions {
    pub plan: PlanPreview,
    previews: Mutex<Vec<Vec<PathBuf>>>,
    finishes: Mutex<Vec<Vec<PathBuf>>>,
}

impl FakeExplorerActions {
    pub fn new(plan: PlanPreview) -> FakeExplorerActions {
        FakeExplorerActions {
            plan,
            previews: Mutex::new(Vec::new()),
            finishes: Mutex::new(Vec::new()),
        }
    }

    pub fn previews(&self) -> Vec<Vec<PathBuf>> {
        self.previews
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn finishes(&self) -> Vec<Vec<PathBuf>> {
        self.finishes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ExplorerActions for FakeExplorerActions {
    fn preview(&self, marks: &[PathBuf], cancel: Arc<AtomicBool>) -> PreviewHandle {
        self.previews
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(marks.to_vec());
        let (tx, rx) = sync_channel(1);
        let _ = tx.send(ScanMsg::Preview(self.plan.clone()));
        PreviewHandle::new(rx, cancel)
    }

    fn finish(&self, marks: &[PathBuf]) -> Handoff {
        self.finishes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(marks.to_vec());
        Handoff::default()
    }
}

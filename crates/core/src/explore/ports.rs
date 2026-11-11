//! The explorer's ports (Task 6.2.1a). All are `Send + Sync` so one shared reference serves the scoped
//! worker threads; `ExtentProbe` lives in [`crate::ports`] and only gains `shared_bytes` there.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use super::msg::{FileRows, PlanPreview, ScanMsg};

/// What a scan is asked to do.
#[derive(Debug, Clone)]
pub struct ScanOpts {
    /// Echoed on every batch; the reducer drops batches of an older generation.
    pub generation: u32,
    /// Follow into other devices (the explorer keeps this off: `-x` semantics).
    pub cross_fs: bool,
    pub threads: usize,
    pub paint_from_cache: bool,
    /// `--max-seconds`: the walk's deadline, counted from `start`.
    pub max_seconds: Option<u64>,
}

impl Default for ScanOpts {
    fn default() -> ScanOpts {
        ScanOpts {
            generation: 0,
            cross_fs: false,
            threads: 4,
            paint_from_cache: false,
            max_seconds: None,
        }
    }
}

/// A running scan: its message channel and the two switches the loop owns.
#[derive(Debug)]
pub struct ScanHandle {
    pub rx: Receiver<ScanMsg>,
    cancel: Arc<AtomicBool>,
    save: Arc<AtomicBool>,
}

impl ScanHandle {
    pub fn new(
        rx: Receiver<ScanMsg>,
        cancel: Arc<AtomicBool>,
        save: Arc<AtomicBool>,
    ) -> ScanHandle {
        ScanHandle { rx, cancel, save }
    }

    /// Stops the walk; what it visited is kept (and saved while the save gate is set).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Cancels and clears the save gate, so no cache file is written for a scan nobody wants.
    pub fn detach(&self) {
        self.save.store(false, Ordering::SeqCst);
        self.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

pub trait ScanSource: Send + Sync {
    /// Starts a walk of `roots`; the receiver is bounded, and dropping it makes the source's sends fail,
    /// which it treats as cancel.
    fn start(&self, roots: &[PathBuf], opts: &ScanOpts) -> ScanHandle;
}

pub trait DirLister: Send + Sync {
    /// The `top_n` files of `dir` by allocated bytes and the totals of the rest; stops early on `cancel`.
    fn list_files(&self, dir: &Path, top_n: usize, cancel: &AtomicBool) -> FileRows;
}

pub trait TermSignals: Send + Sync {
    /// 2, 15, 1 or 3 for SIGINT, SIGTERM, SIGHUP or SIGQUIT once one arrived.
    fn interrupted(&self) -> Option<i32>;
    /// An external SIGTSTP arrived.
    fn suspend_requested(&self) -> bool;
    fn suspend(&self);
}

/// The result of leaving the explorer for the cleaner: the lines to print once the terminal is restored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Handoff {
    pub lines: Vec<OsString>,
    pub exit_code: i32,
}

/// The planner running on a worker; its result arrives as `ScanMsg::Preview` on `rx`.
#[derive(Debug)]
pub struct PreviewHandle {
    pub rx: Receiver<ScanMsg>,
    cancel: Arc<AtomicBool>,
}

impl PreviewHandle {
    pub fn new(rx: Receiver<ScanMsg>, cancel: Arc<AtomicBool>) -> PreviewHandle {
        PreviewHandle { rx, cancel }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// What the TUI calls to leave the screen. The `tui` crate holds no planner, store or renderer, so it
/// cannot delete by itself (`delete` joins this trait in Story 6.6.3).
pub trait ExplorerActions: Send + Sync {
    fn preview(&self, marks: &[PathBuf], cancel: Arc<AtomicBool>) -> PreviewHandle;
    fn finish(&self, marks: &[PathBuf]) -> Handoff;
}

/// Kept so a fake can answer a preview without a channel of its own.
pub fn preview_message(p: PlanPreview) -> ScanMsg {
    ScanMsg::Preview(p)
}

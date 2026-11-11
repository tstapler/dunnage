//! `WalkerScanSource` (Task 6.3.1a): the real [`ScanSource`]. One detached thread runs the walker's
//! `collect_streaming` and sends its batches over a bounded `sync_channel`, so a slow UI slows the walk
//! instead of growing a queue. Quitting never joins the thread (UX-T24): `cancel`/`detach` only set flags.
//! Read-only apart from the cache write the walker itself performs.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use super::cache_paint::paint_batches;
use disk_cleanup_core::explore::msg::{ScanMsg, ScanSummary, StopCause};
use disk_cleanup_core::explore::ports::{ScanHandle, ScanOpts, ScanSource};
use disk_cleanup_scan::index::{
    Backend, CollectRequest, DirEvent, SaveGuard, StreamRequest, DEFAULT_BATCH_MAX,
};

/// Batches that may wait in the channel; the walker blocks on the 65th.
pub const QUEUE_DEPTH: usize = 64;

/// Name of the scan thread; the panic hook below recognises it.
pub const SCAN_THREAD: &str = "explore-scan";

/// Runs once per scan, right after it starts, with the scan roots and a sender on the scan's own channel.
/// The wiring uses it for the side messages (`Headroom`, `Facts`) that share the channel with the batches.
pub type Companion = Arc<dyn Fn(&[PathBuf], SyncSender<ScanMsg>) + Send + Sync>;

/// Walks with a [`Backend`] (the real walker, or a test double).
#[derive(Clone)]
pub struct WalkerScanSource {
    backend: Arc<dyn Backend + Send + Sync>,
    save_guard: Option<SaveGuard>,
    companion: Option<Companion>,
    activity: Option<Arc<AtomicUsize>>,
}

impl WalkerScanSource {
    pub fn new<B: Backend + Send + Sync + 'static>(backend: B) -> WalkerScanSource {
        WalkerScanSource {
            backend: Arc::new(backend),
            save_guard: None,
            companion: None,
            activity: None,
        }
    }

    /// Called at the start of every scan with a sender on its channel (see [`Companion`]).
    #[must_use]
    pub fn with_companion(mut self, companion: Companion) -> WalkerScanSource {
        self.companion = Some(companion);
        self
    }

    /// The counter goes up while a walk is in progress and down before its final message is sent, so the
    /// wiring can answer "is something still being scanned" without seeing the tree.
    #[must_use]
    pub fn with_activity(mut self, activity: Arc<AtomicUsize>) -> WalkerScanSource {
        self.activity = Some(activity);
        self
    }

    /// The free-space policy asked just before the cache is written (see `cache_paint::save_guard`).
    #[must_use]
    pub fn with_save_guard(mut self, guard: SaveGuard) -> WalkerScanSource {
        self.save_guard = Some(guard);
        self
    }
}

static HOOK: Once = Once::new();

/// Installs (once) a panic hook that stays silent for the scan thread and defers to the previous hook for
/// every other thread. A panic on the scan thread is reported to the UI as data (`SourceFailed`); a message
/// on stderr would corrupt the alternate screen.
pub fn silence_scan_thread_panics() {
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if std::thread::current().name() != Some(SCAN_THREAD) {
                previous(info);
            }
        }));
    });
}

impl ScanSource for WalkerScanSource {
    fn start(&self, roots: &[PathBuf], opts: &ScanOpts) -> ScanHandle {
        silence_scan_thread_panics();
        let (tx, rx) = sync_channel(QUEUE_DEPTH);
        let cancel = Arc::new(AtomicBool::new(false));
        let save = Arc::new(AtomicBool::new(true));
        if let Some(c) = &self.companion {
            c(roots, tx.clone());
        }
        let busy = self.activity.as_ref().map(|a| {
            a.fetch_add(1, Ordering::SeqCst);
            Busy(Arc::clone(a))
        });
        let job = Job {
            busy,
            backend: Arc::clone(&self.backend),
            guard: self.save_guard.clone(),
            roots: roots.to_vec(),
            opts: opts.clone(),
            cancel: Arc::clone(&cancel),
            save: Arc::clone(&save),
            tx: tx.clone(),
        };
        let spawned = std::thread::Builder::new()
            .name(SCAN_THREAD.into())
            .spawn(move || job.run());
        if spawned.is_err() {
            let _ = tx.try_send(ScanMsg::SourceFailed(
                "could not start the scan thread".into(),
            ));
        }
        ScanHandle::new(rx, cancel, save)
    }
}

/// Counts a walk as in progress until it is dropped.
struct Busy(Arc<AtomicUsize>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Job {
    busy: Option<Busy>,
    backend: Arc<dyn Backend + Send + Sync>,
    guard: Option<SaveGuard>,
    roots: Vec<PathBuf>,
    opts: ScanOpts,
    cancel: Arc<AtomicBool>,
    save: Arc<AtomicBool>,
    tx: SyncSender<ScanMsg>,
}

impl Job {
    fn run(mut self) {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.walk()));
        self.busy = None;
        let msg = match outcome {
            Ok(Ok(summary)) => ScanMsg::Finished(summary),
            Ok(Err(e)) => ScanMsg::SourceFailed(format!("scan failed: {e}")),
            Err(_) => ScanMsg::SourceFailed("scan thread panicked".into()),
        };
        // A closed channel means nobody is listening any more.
        let _ = self.tx.send(msg);
    }

    fn walk(&self) -> std::io::Result<ScanSummary> {
        let generation = self.opts.generation;
        let collect = CollectRequest {
            roots: &self.roots,
            cross_fs: self.opts.cross_fs,
            deadline: self
                .opts
                .max_seconds
                .map(|s| Instant::now() + Duration::from_secs(s)),
            threads: self.opts.threads,
        };
        let mut req = StreamRequest::new(collect, Arc::clone(&self.cancel));
        req.paint_from_cache = self.opts.paint_from_cache;
        req.save = Arc::clone(&self.save);
        req.batch_max = DEFAULT_BATCH_MAX;
        req.save_guard.clone_from(&self.guard);
        let paints = self.opts.paint_from_cache;
        let gone = AtomicBool::new(false);
        let sink = |events: Vec<DirEvent>| {
            if gone.load(Ordering::SeqCst) {
                return;
            }
            for (mode, group) in paint_batches(events, paints) {
                let msg = ScanMsg::Batch {
                    generation,
                    mode,
                    events: group,
                };
                if self.tx.send(msg).is_err() {
                    // The UI is gone: the walk ends as a cancel instead of waiting on a full queue.
                    gone.store(true, Ordering::SeqCst);
                    self.cancel.store(true, Ordering::SeqCst);
                    return;
                }
            }
        };
        let summary = self.backend.collect_streaming(&req, &sink)?;
        let truncated = summary.truncated;
        let stop = match (truncated, self.cancel.load(Ordering::SeqCst)) {
            (false, _) => StopCause::None,
            (true, true) => StopCause::Cancelled,
            (true, false) => StopCause::Deadline,
        };
        Ok(ScanSummary {
            generation,
            truncated,
            stop,
            errors: summary.errors,
            dirs_listed: summary.dirs_listed,
            dirs_cached: summary.dirs_cached,
            notes: summary.notes,
        })
    }
}

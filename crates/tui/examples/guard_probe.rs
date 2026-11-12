#![allow(clippy::expect_used)]
//! The explorer loop over fakes, run under a real terminal by `tests/pty_restore.rs` (Task 6.5.2c).
//!
//! Usage: `guard_probe <state-dir> [panic|delete]`. It writes its pid to `<state-dir>/pid` before it takes the
//! terminal and its exit code to `<state-dir>/exit` after the loop returns. With `panic`, marking an item
//! panics on the UI thread, which exercises the panic hook. With `delete`, `d` leads to a prompt (each review writes
//! `review-<n>`) and a typed count starts a fake delete that writes `delete-started`, waits for the abort, writes `abort-seen`, takes
//! half a second, writes `worker-done` and reports; the signal tests check that the terminal is restored only
//! after that. Not part of the shipped binary.

use std::ffi::OsString;
use std::io::{self, stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::sync::Arc;

use dunnage_adapters::explore::signals::RealTermSignals;
use dunnage_core::dirs::Dirs;
use dunnage_core::explore::confirm::{ConfirmedPlan, ReviewedPlan};
use dunnage_core::explore::msg::{
    DeleteMsg, DeleteSummary, NodeEvent, PlanPreview, ScanMode, ScanMsg, NO_PARENT,
};
use dunnage_core::explore::ports::{
    DeleteHandle, ExplorerActions, Handoff, PreviewHandle, ScanHandle, ScanOpts, ScanSource,
};
use dunnage_core::explore::testing::{
    reviewed_of, DirInfo, FakeDirLister, FakeExplorerActions, FakeTypedCount, Stats,
};
use dunnage_core::ids::Identity;
use dunnage_core::roots::RootSet;
use dunnage_tui::app::{ExplorerConfig, Ports};
use dunnage_tui::event_loop::{
    run_explorer, CrosstermEvents, IoctlSize, Runtime, SystemClock, Tuning,
};
use dunnage_tui::terminal::{
    fixed_terminal, install_panic_hook, query_size, write_stderr_lines, CrosstermRaw, DiagQueue,
    TerminalGuard,
};
use ratatui::backend::CrosstermBackend;

/// Delivers one root with two children and then nothing, like a scan that is still walking.
struct HoldSource;

impl ScanSource for HoldSource {
    fn start(&self, _roots: &[PathBuf], _opts: &ScanOpts) -> ScanHandle {
        let (tx, rx) = channel();
        let dir = |id: u64, parent: u64, name: &str, subdirs: u32| NodeEvent::Dir {
            id,
            parent,
            name: OsString::from(name),
            dev: 1,
            info: DirInfo {
                stats: Stats {
                    inodes: 2,
                    allocated_bytes: 4096,
                    size_bytes: 10,
                    ..Stats::default()
                },
                ..DirInfo::default()
            },
            subdirs,
            from_cache: false,
            painted: false,
        };
        let _ = tx.send(ScanMsg::Batch {
            generation: 0,
            mode: ScanMode::Fresh,
            events: vec![
                dir(0, NO_PARENT, "/h/code/app", 1_000_000),
                dir(1, 0, "node_modules", 0),
                dir(2, 0, "src", 0),
            ],
        });
        // Keep the sender alive for the life of the process: the scan never finishes.
        std::mem::forget(tx);
        ScanHandle::new(
            rx,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
        )
    }
}

/// Reviews anything, answers a preview at once and runs a delete that waits for the abort (see the header).
struct DeleteProbe {
    state: PathBuf,
    reviews: std::sync::atomic::AtomicUsize,
}

impl ExplorerActions for DeleteProbe {
    fn preview(&self, _marks: &[PathBuf], cancel: Arc<AtomicBool>) -> PreviewHandle {
        let (tx, rx) = channel();
        let _ = tx.send(ScanMsg::Preview(PlanPreview {
            plan_id: "00000000".into(),
            saved: true,
            rows: vec![],
            est_bytes: 0,
            scope_line: String::new(),
            guards_line: String::new(),
        }));
        PreviewHandle::new(rx, cancel)
    }

    fn finish(&self, _marks: &[PathBuf]) -> Handoff {
        Handoff::default()
    }

    fn review(&self, marks: &[PathBuf]) -> Result<ReviewedPlan, String> {
        let n = self
            .reviews
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        std::fs::write(self.state.join(format!("review-{n}")), "1").expect("marker");
        Ok(reviewed_of(marks, 1 << 30))
    }

    fn delete(&self, confirmed: ConfirmedPlan) -> DeleteHandle {
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        let abort = Arc::new(AtomicBool::new(false));
        let (flag, dir, total) = (Arc::clone(&abort), self.state.clone(), confirmed.count());
        std::thread::spawn(move || {
            let _ = tx.send(DeleteMsg::Progress { index: 1, total });
            std::fs::write(dir.join("delete-started"), "1").expect("marker");
            while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            std::fs::write(dir.join("abort-seen"), "1").expect("marker");
            std::thread::sleep(std::time::Duration::from_millis(500));
            std::fs::write(dir.join("worker-done"), "1").expect("marker");
            let _ = tx.send(DeleteMsg::Done(DeleteSummary {
                removed: 0,
                bytes: 0,
                interrupted: true,
            }));
        });
        DeleteHandle::new(rx, abort)
    }
}

fn roots() -> RootSet {
    let dirs = Dirs {
        home: "/h".into(),
        state: "/h/.local/state".into(),
        cache: "/h/.cache".into(),
        config: "/h/.config".into(),
    };
    let canon = |p: &Path| -> io::Result<(PathBuf, Identity)> {
        Ok((
            p.to_path_buf(),
            Identity {
                dev: 1,
                ino: p.as_os_str().len() as u64,
            },
        ))
    };
    RootSet::resolve(
        &dirs,
        &[],
        Some(&[PathBuf::from("/h/code")]),
        Path::new("/h"),
        &canon,
    )
    .expect("fixed roots resolve")
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let state = PathBuf::from(args.next().expect("state dir"));
    let mode = args.next();
    let panic_on_mark = mode.as_ref().is_some_and(|a| a == "panic");
    let delete_mode = mode.as_ref().is_some_and(|a| a == "delete");
    std::fs::write(state.join("pid"), std::process::id().to_string()).expect("pid file");

    let (cols, rows) = query_size().unwrap_or((100, 30));
    let signals = RealTermSignals::new().expect("signal handlers");
    let mut guard = TerminalGuard::enter(stdout(), CrosstermRaw).expect("terminal setup");
    let queue = Arc::new(DiagQueue::default());
    let hook = install_panic_hook(guard.restorer(), Arc::clone(&queue));
    let mut term = fixed_terminal(CrosstermBackend::new(stdout()), cols, rows).expect("terminal");

    let mut cfg = ExplorerConfig::new(vec![PathBuf::from("/h/code/app")], roots());
    if panic_on_mark {
        cfg.mark_check = Arc::new(|_| panic!("forced panic in mark_check"));
    }
    let ports = Ports {
        scan: Arc::new(HoldSource),
        lister: Arc::new(FakeDirLister::default()),
        signals: Arc::new(signals),
        actions: if delete_mode {
            Arc::new(DeleteProbe {
                state: state.clone(),
                reviews: std::sync::atomic::AtomicUsize::new(0),
            })
        } else {
            Arc::new(FakeExplorerActions::new(PlanPreview {
                plan_id: "00000000".into(),
                saved: true,
                rows: vec![],
                est_bytes: 0,
                scope_line: String::new(),
                guards_line: String::new(),
            }))
        },
        typed: Arc::new(FakeTypedCount::new(true)),
    };
    let (mut events, mut sizer, clock) = (CrosstermEvents, IoctlSize, SystemClock::default());
    let exit = {
        let mut rt = Runtime {
            term: &mut term,
            guard: &mut guard,
            events: &mut events,
            sizer: &mut sizer,
            clock: &clock,
            tuning: Tuning::default(),
            diagnostics: Arc::clone(&queue),
        };
        run_explorer(&mut rt, ports, cfg, (cols, rows))
    };
    drop(hook);
    drop(term);
    drop(guard);
    let mut out = stdout();
    for l in &exit.stdout_lines {
        let _ = writeln!(out, "{}", l.to_string_lossy());
    }
    let _ = out.flush();
    write_stderr_lines(&exit.stderr_lines);
    std::fs::write(state.join("exit"), exit.code.to_string()).expect("exit file");
    std::process::exit(exit.code);
}

#![allow(clippy::expect_used)]
//! The explorer loop over fakes, run under a real terminal by `tests/pty_restore.rs` (Task 6.5.2c).
//!
//! Usage: `guard_probe <state-dir> [panic]`. It writes its pid to `<state-dir>/pid` before it takes the
//! terminal and its exit code to `<state-dir>/exit` after the loop returns. With `panic`, marking an item
//! panics on the UI thread, which exercises the panic hook. Not part of the shipped binary.

use std::ffi::OsString;
use std::io::{self, stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::sync::Arc;

use disk_cleanup_adapters::explore::signals::RealTermSignals;
use disk_cleanup_core::dirs::Dirs;
use disk_cleanup_core::explore::msg::{NodeEvent, PlanPreview, ScanMode, ScanMsg, NO_PARENT};
use disk_cleanup_core::explore::ports::{ScanHandle, ScanOpts, ScanSource};
use disk_cleanup_core::explore::testing::{DirInfo, FakeDirLister, FakeExplorerActions, Stats};
use disk_cleanup_core::ids::Identity;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_tui::app::{ExplorerConfig, Ports};
use disk_cleanup_tui::event_loop::{
    run_explorer, CrosstermEvents, IoctlSize, Runtime, SystemClock, Tuning,
};
use disk_cleanup_tui::terminal::{
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
    let panic_on_mark = args.next().is_some_and(|a| a == "panic");
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
        actions: Arc::new(FakeExplorerActions::new(PlanPreview {
            plan_id: "00000000".into(),
            saved: true,
            rows: vec![],
            est_bytes: 0,
            scope_line: String::new(),
            guards_line: String::new(),
        })),
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

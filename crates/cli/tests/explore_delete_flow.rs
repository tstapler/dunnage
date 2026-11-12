#![cfg(feature = "explore")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs,
    non_snake_case
)]
//! Task 6.6.4c: the explorer's delete key end to end over a fixture tree. The TUI loop runs on a
//! `TestBackend` with scripted keys; the review is the real planner over the fixture, and the delete is the
//! public `compose_explorer_apply` sequence the real `delete` runs on a worker (fake audit sink and lock;
//! real `lstat`, mount table and fd-relative directory operations). Nothing outside the tempdir is
//! read for deletion or written.

mod explore_rig;

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dunnage_core::explore::confirm::{ConfirmedPlan, ReviewedPlan};
use dunnage_core::explore::keymap::Key;
use dunnage_core::explore::msg::{
    NodeEvent, PlanPreview, ScanMode, ScanMsg, ScanSummary, NO_PARENT,
};
#[cfg(not(feature = "gate-d"))]
use dunnage_core::explore::ports::DELETE_NOT_IN_BUILD;
use dunnage_core::explore::ports::{DeleteHandle, ExplorerActions, Handoff, PreviewHandle};
use dunnage_core::explore::testing::{
    DirInfo, FakeDirLister, FakeScanSource, FakeTermSignals, FakeTypedCount, Stats,
};
use dunnage_core::AuditRecord;
use dunnage_tui::app::{ExplorerConfig, Ports};
use dunnage_tui::event_loop::{
    run_explorer, EventSource, ExplorerExit, Input, Restore, Runtime, SizeSource, SystemClock,
    Tuning,
};
use dunnage_tui::terminal::{fixed_terminal, DiagQueue};
use dunnage_tui::testing::{Buffer, TestBackend};
use explore_rig::{manifest, Ex, Knobs};

// ----- the loop's harness -----

struct Keys {
    start: Instant,
    queue: VecDeque<(u64, Input)>,
}

impl Keys {
    fn ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap()
    }
}

impl EventSource for Keys {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        let now = self.ms();
        let wait = match self.queue.front() {
            Some((at, _)) if *at <= now => return Ok(true),
            Some((at, _)) => timeout.min(Duration::from_millis(at - now)),
            None => timeout,
        };
        std::thread::sleep(wait);
        let now = self.ms();
        Ok(self.queue.front().is_some_and(|(at, _)| *at <= now))
    }

    fn read(&mut self) -> io::Result<Input> {
        Ok(self.queue.pop_front().unwrap().1)
    }
}

struct NoRestore;

impl Restore for NoRestore {
    fn restore(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn reenter(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct NoSize;

impl SizeSource for NoSize {
    fn size(&mut self) -> Option<(u16, u16)> {
        None
    }
}

fn screen_text(buf: &Buffer) -> String {
    let a = buf.area;
    (0..a.height)
        .map(|y| {
            (0..a.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct Ran {
    exit: ExplorerExit,
    screen: String,
}

fn drive(ex: &Ex, actions: Arc<dyn ExplorerActions>, keys: Vec<(u64, Input)>) -> Ran {
    let app = ex.code.join("app");
    let ev = |id: u64, parent: u64, name: &str, alloc: u64| NodeEvent::Dir {
        id,
        parent,
        name: OsString::from(name),
        dev: 1,
        info: DirInfo {
            stats: Stats {
                inodes: 2,
                files: 1,
                allocated_bytes: alloc,
                size_bytes: alloc,
                ..Stats::default()
            },
            ..DirInfo::default()
        },
        subdirs: 0,
        from_cache: false,
        painted: false,
    };
    let mut root = ev(0, NO_PARENT, app.to_str().unwrap(), 4096);
    if let NodeEvent::Dir { subdirs, .. } = &mut root {
        *subdirs = 3;
    }
    let script = vec![
        ScanMsg::Batch {
            generation: 0,
            mode: ScanMode::Fresh,
            events: vec![
                root,
                ev(1, 0, "dist", 3 << 20),
                ev(2, 0, "out", 2 << 20),
                ev(3, 0, "keep", 1 << 20),
            ],
        },
        ScanMsg::Finished(ScanSummary {
            generation: 0,
            ..ScanSummary::default()
        }),
    ];
    let mut cfg = ExplorerConfig::new(vec![app], ex.roots());
    cfg.home = Some(ex.home.clone());
    cfg.now_unix = 1_800_000_000;
    cfg.audit_log = Some(ex.dirs.state.join("audit.jsonl"));
    let ports = Ports {
        scan: Arc::new(FakeScanSource::new(script)),
        lister: Arc::new(FakeDirLister::default()),
        signals: Arc::new(FakeTermSignals::never()),
        actions,
        typed: Arc::new(FakeTypedCount::new(true)),
    };
    let mut term = fixed_terminal(TestBackend::new(100, 24), 100, 24).unwrap();
    let mut keys = Keys {
        start: Instant::now(),
        queue: keys.into(),
    };
    let (mut guard, mut sizer, clock) = (NoRestore, NoSize, SystemClock::default());
    let exit = {
        let mut rt = Runtime {
            term: &mut term,
            guard: &mut guard,
            events: &mut keys,
            sizer: &mut sizer,
            clock: &clock,
            tuning: Tuning::default(),
            diagnostics: Arc::new(DiagQueue::default()),
        };
        run_explorer(&mut rt, ports, cfg, (100, 24))
    };
    Ran {
        exit,
        screen: screen_text(term.backend().inner().buffer()),
    }
}

fn k(c: char) -> Input {
    Input::Key(Key::Char(c))
}

fn key(key: Key) -> Input {
    Input::Key(key)
}

// ----- the actions over the rig -----

/// `review` is the rig's real planner over the fixture; `delete` runs `compose_explorer_apply` on a worker.
struct RigActions {
    ex: Arc<Ex>,
    exit: Arc<Mutex<Option<i32>>>,
}

impl ExplorerActions for RigActions {
    fn preview(&self, _marks: &[PathBuf], cancel: Arc<AtomicBool>) -> PreviewHandle {
        let (tx, rx) = sync_channel(1);
        let _ = tx.send(ScanMsg::Preview(PlanPreview {
            plan_id: "00000000".into(),
            saved: true,
            rows: vec![],
            est_bytes: 0,
            scope_line: "marked items".into(),
            guards_line: "Guards run now.".into(),
        }));
        PreviewHandle::new(rx, cancel)
    }

    fn finish(&self, _marks: &[PathBuf]) -> Handoff {
        Handoff::default()
    }

    fn review(&self, marks: &[PathBuf]) -> Result<ReviewedPlan, String> {
        Ok(self.ex.review(marks))
    }

    fn delete(&self, confirmed: ConfirmedPlan) -> DeleteHandle {
        let (tx, rx) = sync_channel(64);
        let abort = Arc::new(AtomicBool::new(false));
        let handle = DeleteHandle::new(rx, Arc::clone(&abort));
        let (ex, exit) = (Arc::clone(&self.ex), Arc::clone(&self.exit));
        std::thread::spawn(move || {
            let knobs = Knobs {
                abort: Some(abort),
                events: Some(tx),
                ..Knobs::default()
            };
            let ran = ex.run_with(confirmed, ex.confined_ops(), &knobs);
            *exit.lock().unwrap() = Some(ran.report.exit);
        });
        handle
    }
}

fn rig_actions(ex: &Arc<Ex>) -> (Arc<RigActions>, Arc<Mutex<Option<i32>>>) {
    let exit = Arc::new(Mutex::new(None));
    (
        Arc::new(RigActions {
            ex: Arc::clone(ex),
            exit: Arc::clone(&exit),
        }),
        exit,
    )
}

fn fixture() -> (Arc<Ex>, PathBuf) {
    let ex = Ex::new();
    std::fs::create_dir_all(ex.code.join("app")).unwrap();
    let dist = ex.tree("app/dist", 3, 4000);
    ex.tree("app/out", 2, 4000);
    ex.tree("app/keep", 1, 100);
    (Arc::new(ex), dist)
}

// ----- the cases -----

#[test]
fn safety_an_explorer_delete_of_a_throwaway_directory_removes_exactly_that_directory_and_writes_the_audit(
) {
    let (ex, dist) = fixture();
    let before = manifest(&ex.code);
    let (actions, exit) = rig_actions(&ex);
    // dist is the first row (the largest): mark it, plan, review it, open the prompt, type 1, leave
    let keys = vec![
        (200, k(' ')),
        (500, k('d')),
        (1500, k('d')),
        (2500, k('1')),
        (2700, key(Key::Enter)),
        (4500, key(Key::Esc)),
        (4700, k('q')),
    ];
    let out = drive(&ex, actions, keys);

    assert!(!dist.exists(), "the marked directory is gone");
    let after = manifest(&ex.code);
    let mut expected = before.clone();
    expected.retain(|p, _| !p.starts_with(&dist));
    let mut got = after.clone();
    got.retain(|p, _| !p.starts_with(&dist));
    assert_eq!(expected, got, "everything else is exactly as it was");
    assert!(after.keys().all(|p| !p.starts_with(&dist)));

    let recs = ex.audit.records();
    let has = |f: &dyn Fn(&AuditRecord) -> bool| recs.iter().any(f);
    assert!(has(&|r| matches!(r, AuditRecord::RunStart(_))));
    assert!(has(&|r| matches!(r, AuditRecord::ExplorerConfirm(_))));
    assert!(has(&|r| matches!(r, AuditRecord::Intent(_))));
    assert!(has(&|r| matches!(r, AuditRecord::Result(_))));
    assert!(has(&|r| matches!(r, AuditRecord::RunEnd(_))));
    assert_eq!(*exit.lock().unwrap(), Some(0));

    assert_eq!(out.exit.code, 0);
    let lines: Vec<String> = out
        .exit
        .stdout_lines
        .iter()
        .map(|l| l.to_string_lossy().into_owned())
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("EXPLORE CLOSED - deleted 1 item, "),
        "{lines:?}"
    );
    assert!(lines[0].ends_with("audit.jsonl"), "{lines:?}");
}

#[test]
fn safety_a_wrong_count_at_the_prompt_deletes_nothing_in_the_fixture() {
    let (ex, dist) = fixture();
    let before = manifest(&ex.code);
    let (actions, exit) = rig_actions(&ex);
    let keys = vec![
        (200, k(' ')),
        (500, k('d')),
        (1500, k('d')),
        (2500, k('2')),
        (2700, key(Key::Enter)),
        (3300, key(Key::Ctrl('c'))),
    ];
    let out = drive(&ex, actions, keys);
    assert!(dist.exists());
    assert_eq!(manifest(&ex.code), before, "the fixture is untouched");
    assert!(
        ex.audit.records().is_empty(),
        "no audit line for a refused entry"
    );
    assert_eq!(*exit.lock().unwrap(), None, "the apply never ran");
    assert!(
        out.screen.contains("That is not 1. Nothing was deleted."),
        "{}",
        out.screen
    );
    assert_eq!(out.exit.code, 130);
}

fn real_actions(ex: &Ex) -> dunnage_cli::explore::actions::RealExplorerActions {
    use dunnage_cli::explore::actions::{ExplorerPorts, RealExplorerActions};
    use dunnage_cli::explore::handle::DeleteWiring;
    use dunnage_core::testing::{
        FakeAuditSink, FakeClock, FakeCommandRunner, FakePlanStore, FakeRunGuard, FakeSpaceMeter,
    };
    use dunnage_core::{BuildInfo, RunRoots};

    let ports = ExplorerPorts {
        fs: Arc::new(dunnage_cli::wiring::env::RealFs),
        runner: Arc::new(FakeCommandRunner::new()),
        clock: Arc::new(FakeClock::at(explore_rig::NOW)),
        probe: Arc::new(dunnage_adapters::RealProbe),
        space: Arc::new(FakeSpaceMeter::new(
            vec![FakeSpaceMeter::plain_snapshot("/x", 500, 1000)],
            FakeSpaceMeter::stable(),
        )),
        audit: Arc::new(FakeAuditSink::new()),
        guard: Arc::new(FakeRunGuard::new()),
        plans: Arc::new(FakePlanStore::new()),
        dirs: ex.dirs.clone(),
        targets: Vec::new(),
        build: BuildInfo {
            bin_version: "0.1.0".into(),
            git_commit: "abc".into(),
            dirty: false,
            metascope_rev: "none".into(),
            patched: false,
            metascope_dirty: false,
        },
        roots: RunRoots::default(),
        run_id: "run-1".into(),
        jobs: 1,
    };
    RealExplorerActions::new(Arc::new(ports)).with_delete_wiring(Arc::new(DeleteWiring {
        base: ex.base_dirs.clone(),
        roots: ex.roots(),
        exe: PathBuf::from("/usr/bin/dunnage"),
    }))
}

/// The real `review` and `delete` of the build under test, with the preview answered by the rig: the
/// preview needs the cleaner's kinds, which a fixture without a registry does not have.
struct Hybrid(dunnage_cli::explore::actions::RealExplorerActions);

impl ExplorerActions for Hybrid {
    fn preview(&self, marks: &[PathBuf], cancel: Arc<AtomicBool>) -> PreviewHandle {
        let (tx, rx) = sync_channel(1);
        let _ = tx.send(ScanMsg::Preview(PlanPreview {
            plan_id: "00000000".into(),
            saved: true,
            rows: vec![],
            est_bytes: 0,
            scope_line: format!("{} marked", marks.len()),
            guards_line: "Guards run now.".into(),
        }));
        PreviewHandle::new(rx, cancel)
    }

    fn finish(&self, marks: &[PathBuf]) -> Handoff {
        self.0.finish(marks)
    }

    fn review(&self, marks: &[PathBuf]) -> Result<ReviewedPlan, String> {
        ExplorerActions::review(&self.0, marks)
    }

    fn delete(&self, confirmed: ConfirmedPlan) -> DeleteHandle {
        self.0.delete(confirmed)
    }
}

#[cfg(not(feature = "gate-d"))]
#[test]
fn safety_in_the_default_build_d_shows_the_refusal_and_nothing_is_deleted() {
    let (ex, dist) = fixture();
    let before = manifest(&ex.code);
    let keys = vec![
        (200, k(' ')),
        (500, k('d')),
        (1500, k('d')),
        (2200, key(Key::Ctrl('c'))),
    ];
    let out = drive(&ex, Arc::new(Hybrid(real_actions(&ex))), keys);
    assert!(
        out.screen.contains(DELETE_NOT_IN_BUILD),
        "d must say why:\n{}",
        out.screen
    );
    assert!(!out.screen.contains("DELETE - this cannot be undone"));
    assert!(dist.exists());
    assert_eq!(manifest(&ex.code), before);
    assert_eq!(out.exit.code, 130);
}

#[cfg(feature = "gate-d")]
#[test]
fn safety_in_a_gate_d_build_the_real_review_opens_the_prompt_and_a_cancel_deletes_nothing() {
    let (ex, dist) = fixture();
    let before = manifest(&ex.code);
    let keys = vec![
        (200, k(' ')),
        (500, k('d')),
        (3500, k('d')),
        (7000, key(Key::Esc)),
        (7500, k('q')),
        (7800, k('x')),
    ];
    let out = drive(&ex, Arc::new(Hybrid(real_actions(&ex))), keys);
    assert!(dist.exists());
    assert_eq!(manifest(&ex.code), before, "the fixture is untouched");
    assert_eq!(out.exit.code, 0);
    assert_eq!(
        out.exit.stdout_lines,
        vec![OsString::from("EXPLORE CLOSED - nothing was deleted.")]
    );
}

#[cfg(feature = "gate-d")]
#[test]
fn safety_in_a_gate_d_build_the_prompt_shows_the_real_count_and_total() {
    let (ex, _dist) = fixture();
    let keys = vec![
        (200, k(' ')),
        (500, k('d')),
        (3500, k('d')),
        (7000, key(Key::Ctrl('c'))),
    ];
    let out = drive(&ex, Arc::new(Hybrid(real_actions(&ex))), keys);
    assert!(
        out.screen.contains("DELETE - this cannot be undone"),
        "{}",
        out.screen
    );
    assert!(
        out.screen
            .contains("Type 1 and press Enter to delete, Esc to cancel: _"),
        "{}",
        out.screen
    );
}

// ----- --text and --json never delete (UX-T26) -----

#[test]
fn safety_text_and_json_have_no_path_to_the_delete_and_leave_the_fixture_untouched() {
    use dunnage_cli::explore::json::run_json;
    use dunnage_cli::explore::text::{run_text, ExploreEnv};

    let (ex, dist) = fixture();
    let before = manifest(&ex.code);
    let app = ex.code.join("app");
    // `ExploreEnv` is everything `run_text` and `run_json` read: a scan source, the signals, the roots and
    // the classification roots. It has no actions, no confirmation and no directory operations, so there is
    // nothing in it that could delete; this builds one and runs both views over the fixture.
    let env = || {
        ExploreEnv::new(
            Arc::new(dunnage_core::explore::testing::FakeScanSource::new(vec![
                ScanMsg::Finished(ScanSummary::default()),
            ])),
            Arc::new(FakeTermSignals::never()),
            vec![app.clone()],
            ex.roots(),
        )
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let _ = run_text(&env(), &mut out, &mut err);
    let _ = run_json(&env(), &mut out, &mut err);
    assert!(dist.exists());
    assert_eq!(manifest(&ex.code), before);
}

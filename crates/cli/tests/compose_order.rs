#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 2.3.4, Task 2.3.4a: the order of an apply (`design/ux.md` 1.4 and 3.1; UX-13) as a tested sequence
//! over fakes with a `Trace`, then the same refusals through the spawned binary.

mod common;
mod rig;
mod support;

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::Fixture;
use disk_cleanup_cli::app::{ApplyMode, Ports};
use disk_cleanup_cli::terminal::Terminal;
use disk_cleanup_cli::wiring::audit::{GateExit, StampOutcome};
use disk_cleanup_cli::wiring::compose::{compose_apply, ApplyState, ApplyWiring, Sink, Trace};
use disk_cleanup_core::candidate::BuildDirFlavor;
use disk_cleanup_core::ports::Env;
use disk_cleanup_core::selector::Selection;
use disk_cleanup_core::snapshot_types::{AllocSnapshot, SpaceUse};
use disk_cleanup_core::target::{Guarded, KindGate, Target};
use disk_cleanup_core::testing::{AuditCall, FakeAuditSink, FakeRunGuard, FakeSpaceMeter};
use disk_cleanup_core::{
    ApplyToken, Candidate, DeleteFailure, FatalError, Freed, RunRoots, SelectorId, SkipReason,
};
use rig::{build, FakeTarget, Rig};
use support::{build_dir, cache, GIB};

const CACHES: &[SelectorId] = &[SelectorId::Caches];
const BUILD_DIRS: &[SelectorId] = &[SelectorId::BuildDirs];
const VOLUMES: &[SelectorId] = &[SelectorId::DockerVolumes];

/// A writer both the hooks and the renderer can hold, readable afterwards.
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Shared {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// The state directory fake: counts what the apply asked of it.
#[derive(Default)]
struct FakeState {
    unclean: AtomicUsize,
    reserves: AtomicUsize,
    reserve_error: Option<i32>,
}

impl ApplyState for FakeState {
    fn unclean_notice(&self) -> Result<Option<String>, String> {
        self.unclean.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }

    fn ensure_reserves(&self) -> Result<Option<String>, FatalError> {
        self.reserves.fetch_add(1, Ordering::SeqCst);
        match self.reserve_error {
            Some(errno) => Err(FatalError::AuditUnwritable {
                path: PathBuf::from("/home/u/.local/state/disk-cleanup/audit.jsonl"),
                errno,
            }),
            None => Ok(None),
        }
    }
}

/// A kind whose daemon is down: every selector it serves is unavailable.
struct DownTarget;

impl Target for DownTarget {
    fn name(&self) -> &'static str {
        "docker"
    }

    fn selectors(&self) -> &'static [SelectorId] {
        VOLUMES
    }

    fn gate(&self, _env: &Env) -> KindGate {
        KindGate::Unavailable(SkipReason::DaemonUnavailable)
    }

    fn discover(&self, _env: &Env, _sel: &Selection) -> Vec<Candidate> {
        Vec::new()
    }

    fn guard(&self, _c: &Candidate, _env: &Env) -> Guarded {
        Guarded::Skip(SkipReason::DaemonUnavailable)
    }

    fn execute(
        &self,
        _c: &Candidate,
        _env: &Env,
        _proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        panic!("a down kind never executes");
    }
}

struct Ran {
    code: i32,
    stdout: String,
    stderr: String,
    trace: Trace,
}

type Stamp = dyn Fn(char) -> Result<StampOutcome, GateExit>;

#[allow(clippy::unnecessary_wraps)] // the signature is the `stamp` port's
fn ok_stamp(_gate: char) -> Result<StampOutcome, GateExit> {
    Ok(StampOutcome::Confined)
}

fn go(
    rig: &Rig,
    state: &FakeState,
    targets: &[&dyn Target],
    argv: &[&str],
    stamp: &Stamp,
    stderr_tty: bool,
) -> Ran {
    let env = rig.env();
    let mint = ApplyToken::for_tests;
    let outcome = |_m, _v, _f, _u| {};
    let ports = Ports {
        env: &env,
        targets: targets.to_vec(),
        apply: ApplyMode::Granted(&mint),
        apply_capable: None,
        stamp,
        build: build(),
        roots: RunRoots::default(),
        scan_roots: 1,
        exe: PathBuf::from("/usr/bin/disk-cleanup"),
        term: Terminal::pipe(),
        stdout_tty: false,
        run_id: "run-1".into(),
        via: None,
        outcome: &outcome,
        reserves_note: None,
        config_roots: None,
    };
    let (out, err) = (Shared::default(), Shared::default());
    let mut trace = Trace::default();
    let mut args: Vec<OsString> = vec!["disk-cleanup".into()];
    args.extend(argv.iter().map(OsString::from));
    let code = compose_apply(
        args.into_iter().skip(1).collect(),
        &ApplyWiring {
            ports: &ports,
            state,
            stderr_tty,
        },
        &Sink::new(out.clone()),
        &Sink::new(err.clone()),
        &mut trace,
    );
    Ran {
        code,
        stdout: out.text(),
        stderr: err.text(),
        trace,
    }
}

fn caches() -> FakeTarget {
    FakeTarget::new(
        CACHES,
        vec![
            cache("/home/u/.npm/_cacache", 1.0),
            cache("/home/u/.cargo/registry/src", 0.5),
        ],
    )
}

const FULL: [&str; 15] = [
    "parse_args",
    "kind_availability",
    "check_stamp",
    "plan_id_gate",
    "unclean_notice",
    "marker",
    "ensure_reserves",
    "run_lock",
    "audit_open",
    "starvation_preflight",
    "header_rest",
    "execute",
    "run_end",
    "summary",
    "closing",
];

fn two_lines(stdout: &str) -> Vec<&str> {
    stdout.lines().collect()
}

#[test]
fn a_full_apply_should_run_the_steps_in_the_order_of_the_ux_doc_When_caches_are_applied() {
    let (rig, state, c) = (Rig::new(), FakeState::default(), caches());
    let r = go(
        &rig,
        &state,
        &[&c],
        &["--apply", "--kinds", "caches"],
        &ok_stamp,
        false,
    );
    assert_eq!(r.code, 0, "{} {}", r.stdout, r.stderr);
    assert_eq!(r.trace.steps(), FULL);
    assert!(
        r.stdout.starts_with("APPLYING - deleting. plan-id "),
        "{}",
        r.stdout
    );
    assert_eq!(
        r.stdout.matches("APPLYING - deleting.").count(),
        1,
        "the marker is printed once: live, not again by the final screen\n{}",
        r.stdout
    );
    assert!(r.stdout.trim_end().ends_with("APPLY FINISHED - exit 0."));
    assert_eq!(state.reserves.load(Ordering::SeqCst), 1);
    assert_eq!(state.unclean.load(Ordering::SeqCst), 1);
}

#[test]
fn a_missing_stamp_should_end_the_run_at_check_stamp_with_no_state_and_empty_stdout() {
    let (rig, state, c) = (Rig::new(), FakeState::default(), caches());
    let stamp = |_g: char| {
        Err(GateExit {
            text: "error: no stamp was installed. Nothing was deleted.".into(),
            json_code: "stamp_refused",
        })
    };
    let r = go(
        &rig,
        &state,
        &[&c],
        &["--apply", "--kinds", "caches"],
        &stamp,
        false,
    );
    assert_eq!(r.code, 64);
    assert_eq!(r.trace.steps(), &FULL[..3]);
    assert!(r.stdout.is_empty(), "{}", r.stdout);
    assert!(r.stderr.contains("no stamp was installed"));
    assert_eq!(
        state.reserves.load(Ordering::SeqCst),
        0,
        "no reserve before step 4"
    );
    assert_eq!(state.unclean.load(Ordering::SeqCst), 0);
    assert!(rig.audit.calls().is_empty());
    assert_eq!(rig.guard.acquire_calls(), 0);
    assert!(c.executed().is_empty());
}

#[test]
fn the_plan_id_gate_should_end_the_run_at_plan_id_gate_with_the_same_empty_state() {
    let rig = Rig::new();
    let state = FakeState::default();
    let t = FakeTarget::new(
        BUILD_DIRS,
        vec![build_dir(
            "/home/u/code/a/node_modules",
            2.0,
            40,
            BuildDirFlavor::NodeModules,
        )],
    );
    let r = go(
        &rig,
        &state,
        &[&t],
        &["--apply", "--kinds", "build-dirs"],
        &ok_stamp,
        false,
    );
    assert_eq!(r.code, 64, "{}", r.stderr);
    assert_eq!(r.trace.steps(), &FULL[..4]);
    assert!(r.stdout.is_empty(), "{}", r.stdout);
    assert_eq!(state.reserves.load(Ordering::SeqCst), 0);
    assert_eq!(state.unclean.load(Ordering::SeqCst), 0);
    assert!(rig.audit.calls().is_empty());
    assert_eq!(rig.guard.acquire_calls(), 0);
    assert!(t.executed().is_empty());
}

#[test]
fn every_selected_selector_unavailable_should_end_the_run_at_kind_availability_with_exit_1() {
    let (rig, state) = (Rig::new(), FakeState::default());
    let r = go(
        &rig,
        &state,
        &[&DownTarget],
        &["--apply", "--kinds", "docker-volumes"],
        &ok_stamp,
        false,
    );
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert_eq!(r.trace.steps(), &FULL[..2]);
    assert!(
        r.stdout.is_empty(),
        "no marker: the run did not get past step 4\n{}",
        r.stdout
    );
    assert!(
        r.stderr
            .contains("docker: could not run (daemon not reachable"),
        "{}",
        r.stderr
    );
    assert_eq!(state.reserves.load(Ordering::SeqCst), 0);
    assert!(rig.audit.calls().is_empty());
}

fn finished_with(r: &Ran, steps: &[&str], exit: i32) {
    assert_eq!(r.code, exit, "{} {}", r.stdout, r.stderr);
    assert_eq!(r.trace.steps(), steps);
    let lines = two_lines(&r.stdout);
    assert_eq!(lines.len(), 2, "{:?}", r.stdout);
    assert!(
        lines[0].starts_with("APPLYING - deleting. plan-id "),
        "{lines:?}"
    );
    assert_eq!(lines[1], format!("APPLY FINISHED - exit {exit}."));
}

#[test]
fn a_held_run_lock_should_print_the_marker_and_the_closing_line_only_with_exit_2() {
    let (mut rig, state, c) = (Rig::new(), FakeState::default(), caches());
    rig.guard = Arc::new(FakeRunGuard::lock_held_by(4242));
    let r = go(
        &rig,
        &state,
        &[&c],
        &["--apply", "--kinds", "caches"],
        &ok_stamp,
        false,
    );
    finished_with(
        &r,
        &[
            "parse_args",
            "kind_availability",
            "check_stamp",
            "plan_id_gate",
            "unclean_notice",
            "marker",
            "ensure_reserves",
            "run_lock",
            "closing",
        ],
        2,
    );
    assert!(
        r.stderr
            .contains("another disk-cleanup run is active (pid 4242)"),
        "{}",
        r.stderr
    );
    assert!(rig.audit.calls().is_empty(), "no run_start before the lock");
}

#[test]
fn an_unwritable_audit_log_should_refuse_with_exit_2_and_no_intent() {
    let (mut rig, state, c) = (Rig::new(), FakeState::default(), caches());
    rig.audit = FakeAuditSink::new().fail_nth(AuditCall::RunStart, 1);
    let r = go(
        &rig,
        &state,
        &[&c],
        &["--apply", "--kinds", "caches"],
        &ok_stamp,
        false,
    );
    finished_with(&r, &[&FULL[..9], &["closing"]].concat(), 2);
    assert!(
        r.stderr.contains("cannot write the audit log"),
        "{}",
        r.stderr
    );
    assert!(!rig.audit.calls().contains(&AuditCall::Intent));
    assert!(c.executed().is_empty());
}

#[test]
fn a_starved_filesystem_should_refuse_with_exit_3_after_the_audit_opened_and_before_any_intent() {
    let mut rig = Rig::new();
    let space = |used, total| SpaceUse {
        bytes_used: used,
        total_bytes: total,
    };
    let mut snap = FakeSpaceMeter::plain_snapshot("/home/u", 900 * GIB, 1000 * GIB);
    snap.fs_type = "btrfs".into();
    snap.btrfs = Some(AllocSnapshot {
        data: space(900 * GIB, 950 * GIB),
        metadata: space(10 * GIB, 10 * GIB + (10 << 20)),
        system: space(1 << 20, 8 << 20),
        unallocated: 100 << 20,
    });
    rig.space = FakeSpaceMeter::new(vec![snap], FakeSpaceMeter::stable());
    let (state, c) = (FakeState::default(), caches());
    let r = go(
        &rig,
        &state,
        &[&c],
        &["--apply", "--kinds", "caches"],
        &ok_stamp,
        false,
    );
    finished_with(&r, &[&FULL[..10], &["closing"]].concat(), 3);
    assert!(
        r.stderr.contains("deleting could fail with ENOSPC"),
        "{}",
        r.stderr
    );
    assert_eq!(
        rig.audit.calls(),
        [AuditCall::RunStart, AuditCall::RunEnd],
        "the refused run is closed in the log: no intent, and no 'previous run did not finish' next time"
    );
    assert!(c.executed().is_empty());
}

#[test]
fn reserves_that_cannot_be_created_should_refuse_with_exit_2_before_the_lock() {
    let (rig, c) = (Rig::new(), caches());
    let state = FakeState {
        reserve_error: Some(30),
        ..FakeState::default()
    };
    let r = go(
        &rig,
        &state,
        &[&c],
        &["--apply", "--kinds", "caches"],
        &ok_stamp,
        false,
    );
    finished_with(&r, &[&FULL[..7], &["closing"]].concat(), 2);
    assert_eq!(rig.guard.acquire_calls(), 0);
}

#[test]
fn the_progress_hook_should_print_one_removing_line_per_item_on_stderr_When_stdout_is_redirected() {
    let (rig, state, c) = (Rig::new(), FakeState::default(), caches());
    let r = go(
        &rig,
        &state,
        &[&c],
        &["--apply", "--kinds", "caches"],
        &ok_stamp,
        true,
    );
    assert_eq!(r.code, 0);
    // Workers start in either order, so the first line names whichever item began first.
    assert!(
        r.stderr.contains("[1/2] removing /home/u/.npm/_cacache")
            || r.stderr
                .contains("[1/2] removing /home/u/.cargo/registry/src"),
        "{}",
        r.stderr
    );
    // The 2 s gap rule drops the second line of a fast run.
    assert!(!r.stderr.contains("[2/2]"), "{}", r.stderr);
    let quiet = go(
        &rig,
        &state,
        &[&c],
        &["--apply", "--kinds", "caches", "--quiet"],
        &ok_stamp,
        true,
    );
    assert!(!quiet.stderr.contains("removing"), "{}", quiet.stderr);
}

#[test]
fn a_json_apply_should_print_no_marker_and_one_document() {
    let (rig, state, c) = (Rig::new(), FakeState::default(), caches());
    let r = go(
        &rig,
        &state,
        &[&c],
        &["--apply", "--kinds", "caches", "--json"],
        &ok_stamp,
        false,
    );
    assert_eq!(r.code, 0);
    let doc: serde_json::Value = serde_json::from_str(&r.stdout).expect("one JSON document");
    assert_eq!(doc["exit_code"], 0);
    assert_eq!(r.trace.steps(), FULL);
}

// ---------------------------------------------------------------- the spawned binary

fn text(out: &std::process::Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn stale_fixture(f: &Fixture) -> PathBuf {
    let root = f.home().join("code/proj");
    let stale = root.join("stale-a");
    std::fs::create_dir_all(stale.join("sub")).unwrap();
    std::fs::write(stale.join("sub/blob"), vec![1u8; 4096]).unwrap();
    root
}

fn dry_run_id(f: &Fixture, root: &std::path::Path) -> String {
    let out = f
        .fixture_cmd()
        .args(["--kinds", "fixture-destructive", "--root"])
        .arg(root)
        .arg("--json")
        .output()
        .unwrap();
    common::extract_plan_id(&String::from_utf8_lossy(&out.stdout))
        .unwrap_or_else(|| panic!("{}", String::from_utf8_lossy(&out.stdout)))
}

fn apply_cmd(f: &Fixture, root: &std::path::Path, id: &str) -> std::process::Command {
    let mut c = f.fixture_cmd();
    c.args(["--apply", "--kinds", "fixture-destructive", "--root"])
        .arg(root)
        .args(["--plan-id", id]);
    c
}

#[test]
fn the_binary_should_print_two_lines_and_exit_2_when_another_process_holds_the_run_lock() {
    let f = Fixture::new();
    let root = stale_fixture(&f);
    let id = dry_run_id(&f, &root);
    // A second process holds run.lock: `flock` from util-linux keeps it for the life of the child.
    let lock = f.xdg("state").join("disk-cleanup/run.lock");
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let mut holder = std::process::Command::new("flock")
        .arg("--exclusive")
        .arg(&lock)
        .args(["sleep", "30"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("util-linux flock");
    std::thread::sleep(std::time::Duration::from_millis(400));
    let out = apply_cmd(&f, &root, &id).output().unwrap();
    holder.kill().unwrap();
    let _ = holder.wait();
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{stdout:?}");
    assert!(lines[0].starts_with("APPLYING - deleting. plan-id "));
    assert_eq!(lines[1], "APPLY FINISHED - exit 2.");
    assert!(
        stderr.contains("another disk-cleanup run is active"),
        "{stderr}"
    );
    assert!(root.join("stale-a").is_dir(), "nothing was deleted");
}

#[test]
fn the_binary_should_print_two_lines_and_exit_2_when_the_audit_log_is_read_only() {
    let f = Fixture::new();
    let root = stale_fixture(&f);
    let id = dry_run_id(&f, &root);
    let audit = f.xdg("state").join("disk-cleanup/audit.jsonl");
    std::fs::write(&audit, "").unwrap();
    std::fs::set_permissions(&audit, std::os::unix::fs::PermissionsExt::from_mode(0o400)).unwrap();
    if std::fs::OpenOptions::new()
        .append(true)
        .open(&audit)
        .is_ok()
    {
        eprintln!("skipped: this user can write a mode 0400 file (root?)");
        return;
    }
    let out = apply_cmd(&f, &root, &id).output().unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert_eq!(stdout.lines().count(), 2, "{stdout:?}");
    assert!(stderr.contains("cannot write the audit log"), "{stderr}");
    assert!(root.join("stale-a").is_dir());
}

#[test]
fn the_binary_should_print_two_lines_and_exit_3_when_the_filesystem_is_starved() {
    let f = Fixture::new();
    let root = stale_fixture(&f);
    let id = dry_run_id(&f, &root);
    let out = apply_cmd(&f, &root, &id)
        .env("DC_FAKE_STARVED", "1")
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    // `DC_FAKE_STARVED` is read only by a `test-support` build (the flavor of this test binary).
    assert_eq!(out.status.code(), Some(3), "{stderr}");
    assert_eq!(stdout.lines().count(), 2, "{stdout:?}");
    assert!(
        stderr.contains("deleting could fail with ENOSPC"),
        "{stderr}"
    );
    assert!(root.join("stale-a").is_dir());
}

#[test]
fn a_refused_apply_should_leave_no_state_file_in_steps_1_to_4() {
    let f = Fixture::new();
    let root = stale_fixture(&f);
    let state = f.xdg("state").join("disk-cleanup");
    // Step 4: a destructive kind without a plan-id.
    let out = f
        .fixture_cmd()
        .args(["--apply", "--kinds", "fixture-destructive", "--root"])
        .arg(&root)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(64), "{}", text(&out).1);
    assert!(out.stdout.is_empty());
    assert!(!state.exists(), "a refused run created {state:?}");
    // Step 1: a malformed id.
    let out = f
        .fixture_cmd()
        .args([
            "--apply",
            "--kinds",
            "fixture-destructive",
            "--plan-id",
            "zz",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(64));
    assert!(!state.exists());
}

#[test]
fn the_fixture_destructive_flow_should_need_the_plan_id_and_remove_only_the_stale_directory() {
    let f = Fixture::new();
    let root = stale_fixture(&f);
    std::fs::create_dir_all(root.join("keep-me")).unwrap();
    let id = dry_run_id(&f, &root);

    // One hex digit off: refused as changed/unknown, nothing deleted.
    let mut wrong: Vec<char> = id.chars().collect();
    wrong[0] = if wrong[0] == '0' { '1' } else { '0' };
    let wrong: String = wrong.into_iter().collect();
    let out = apply_cmd(&f, &root, &wrong).output().unwrap();
    assert_eq!(out.status.code(), Some(64), "{}", text(&out).1);
    assert!(out.stdout.is_empty());
    assert!(root.join("stale-a").is_dir());

    let out = apply_cmd(&f, &root, &id).output().unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(stdout.contains("APPLY FINISHED - exit 0."), "{stdout}");
    assert!(
        !root.join("stale-a").exists(),
        "the stale directory is gone"
    );
    assert!(
        root.join("keep-me").is_dir(),
        "only stale-* directories are touched"
    );
    // The spent id: the same command again is refused as changed (the item is gone).
    let again = apply_cmd(&f, &root, &id).output().unwrap();
    assert_eq!(again.status.code(), Some(64));
}

#[test]
fn only_should_name_an_item_of_the_selected_kinds_or_end_the_run_with_exit_64() {
    let (rig, state, c) = (Rig::new(), FakeState::default(), caches());
    let r = go(
        &rig,
        &state,
        &[&c],
        &[
            "--apply",
            "--kinds",
            "caches",
            "--only",
            "/home/u/.npm/_cache",
        ],
        &ok_stamp,
        false,
    );
    assert_eq!(r.code, 64, "{}", r.stderr);
    assert!(r.stdout.is_empty(), "{}", r.stdout);
    assert!(
        r.stderr.starts_with(
            "error: --only /home/u/.npm/_cache is not an item of the selected kinds (caches)."
        ),
        "{}",
        r.stderr
    );
    assert!(r.stderr.ends_with("Nothing was deleted.\n"), "{}", r.stderr);
    assert!(c.executed().is_empty());
    assert_eq!(state.reserves.load(Ordering::SeqCst), 0);

    let ok = go(
        &rig,
        &state,
        &[&c],
        &[
            "--apply",
            "--kinds",
            "caches",
            "--only",
            "/home/u/.npm/_cacache",
        ],
        &ok_stamp,
        false,
    );
    assert_eq!(ok.code, 0, "{}", ok.stderr);
    assert_eq!(
        c.executed().len(),
        1,
        "--only limits the apply to the one item"
    );
}

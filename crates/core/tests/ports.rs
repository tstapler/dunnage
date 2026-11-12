#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Behaviour of the port doubles. Tests that need a `WorktreeRemoval` wait for Story 1.1.3: its only
//! constructor chain starts at `Allowed::new`, which has a single permitted call site (`guard.rs`).

use dunnage_core::ports::{
    Clock, Cmd, CmdOutput, CommandRunner, DryRunRunner, Fs, PlanStore, RunGuard, RunState,
    SpaceMeter, SystemProbe,
};
use dunnage_core::testing::{
    AuditCall, CallKind, EventLog, FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs,
    FakePlanStore, FakeProbe, FakeRunGuard, FakeSpaceMeter, MeterCall,
};
use dunnage_core::{
    ApplyToken, DeleteError, FatalError, MountEntry, Platform, SaveOutcome, SavedSelection, Tri,
};
use std::path::Path;
use std::time::Duration;

fn volume_rm(mutating: bool) -> Cmd {
    Cmd::new("docker", ["volume", "rm", "x"], mutating)
}

#[test]
fn dry_run_runner_should_refuse_run_mutating_When_called() {
    let fake = FakeCommandRunner::new();
    fake.on(
        &["docker", "image", "rm", "sha256:ab12"],
        Ok(CmdOutput::default()),
    );
    let dry = DryRunRunner(fake);
    let cmd = Cmd::new("docker", ["image", "rm", "sha256:ab12"], true);
    assert_eq!(
        dry.run_mutating(&cmd, &ApplyToken::for_tests()),
        Err(DeleteError::Refused)
    );
    assert!(dry.0.calls().is_empty());
}

#[test]
fn dry_run_runner_should_refuse_mislabelled_cmd_When_volume_rm_labelled_not_mutating() {
    let dry = DryRunRunner(FakeCommandRunner::new());
    assert_eq!(dry.run(&volume_rm(false)), Err(DeleteError::Refused));
    assert!(dry.0.calls().is_empty());
}

#[test]
fn dry_run_runner_should_forward_read_only_rows() {
    let fake = FakeCommandRunner::new();
    fake.on(&["docker", "info"], Ok(CmdOutput::default()));
    let dry = DryRunRunner(fake);
    assert!(dry.run(&Cmd::new("docker", ["info"], false)).is_ok());
    assert_eq!(dry.0.calls_of(CallKind::Run).len(), 1);
}

#[test]
#[should_panic(expected = "docker volume rm x")]
fn fake_command_runner_should_panic_with_the_argv_When_a_mutating_cmd_has_no_expectation() {
    let _ = FakeCommandRunner::new().run_mutating(&volume_rm(true), &ApplyToken::for_tests());
}

#[test]
fn fake_command_runner_should_refuse_the_wrong_door_without_recording() {
    let fake = FakeCommandRunner::new();
    assert_eq!(fake.run(&volume_rm(true)), Err(DeleteError::Refused));
    assert_eq!(
        fake.run_mutating(
            &Cmd::new("docker", ["info"], true),
            &ApplyToken::for_tests()
        ),
        Err(DeleteError::Refused)
    );
    assert!(fake.calls().is_empty());
}

#[test]
fn fake_command_runner_should_return_replies_in_order_and_repeat_the_last() {
    let fake = FakeCommandRunner::new();
    fake.on(&["docker", "info"], Err(DeleteError::Timeout));
    fake.on(&["docker", "info"], Ok(CmdOutput::default()));
    let c = Cmd::new("docker", ["info"], false);
    assert_eq!(fake.run(&c), Err(DeleteError::Timeout));
    assert!(fake.run(&c).is_ok());
    assert!(fake.run(&c).is_ok());
}

#[test]
fn exists_should_be_false_without_spawning_When_program_registered_absent() {
    let fake = FakeCommandRunner::new();
    fake.absent("docker");
    assert!(!fake.exists("docker"));
    assert!(fake.exists("git"));
    assert!(fake.calls().is_empty());
}

#[test]
fn fake_probe_should_report_the_scripted_facts_When_proc_is_unreadable() {
    let p = FakeProbe::new()
        .with_euid(1000)
        .with_platform(Platform::Linux)
        .process("steam", Tri::Running)
        .proc_unreadable()
        .with_mounts(vec![MountEntry {
            mount_point: "/var/log".into(),
            dev: 26,
            mnt_id: 7,
        }]);
    let path = Path::new("/h/app/target");
    assert_eq!(p.euid(), 1000);
    assert_eq!(p.platform(), Platform::Linux);
    assert_eq!(p.process_named("steam"), Tri::Running);
    assert_eq!(p.cwd_in_use(path), (Tri::Unknown, None));
    assert_eq!(p.run_state(path), RunState::Unknown);
    assert_eq!(p.mounts().unwrap()[0].mnt_id, 7);
}

#[test]
fn fake_plan_store_should_return_none_until_saved_and_record_the_save_call() {
    let s = FakePlanStore::new();
    assert!(s.load_last().is_none());
    let e = SavedSelection {
        plan_id: "91be04d7".into(),
        saved_at: 0,
        euid: 0,
        kinds: "caches".into(),
        flags: String::new(),
        items: vec![],
        items_truncated: false,
    };
    assert_eq!(s.save_merged(vec![e], 1000, 5000), SaveOutcome::Saved);
    assert_eq!(s.save_calls().len(), 1);
    let saved = s.load_last().unwrap();
    assert_eq!(
        (saved.entries[0].saved_at, saved.entries[0].euid),
        (5000, 1000)
    );
}

#[test]
fn fake_plan_store_should_report_not_saved_When_failing_with_enospc() {
    let s = FakePlanStore::failing(28);
    assert_eq!(s.save_merged(vec![], 1000, 1), SaveOutcome::NotSaved(28));
    assert!(s.load_last().is_none());
}

#[test]
fn fake_audit_sink_should_fail_the_second_intent_and_not_record_it() {
    use dunnage_core::ports::AuditSink;
    use dunnage_core::{CandidateId, Intent, Kind, Locator, SelectorId};
    let sink = FakeAuditSink::new().fail_nth(AuditCall::Intent, 2);
    let locator = Locator::Path("/h/.npm/_cacache".into());
    let intent = Intent {
        run_id: "r".into(),
        ts: 1,
        id: CandidateId::new(Kind::Cache, &locator),
        kind: Kind::Cache,
        selector: SelectorId::Caches,
        locator,
        est_bytes: 1,
        argv: Vec::new(),
    };
    assert!(sink.intent(&intent).is_ok());
    assert_eq!(sink.intent(&intent).unwrap_err().raw_os_error(), Some(5));
    assert!(sink.intent(&intent).is_ok());
    assert_eq!(sink.calls(), vec![AuditCall::Intent; 3]);
    assert_eq!(sink.records().len(), 2);
}

#[test]
fn fake_run_guard_should_hold_the_lock_and_carry_the_signal() {
    let g = FakeRunGuard::new();
    assert!(g.acquire().is_ok());
    assert_eq!(g.interrupted(), None);
    g.interrupt(15);
    assert!(g.abort_flag().is_set());
    assert_eq!(g.interrupted(), Some(15));
    assert_eq!(
        FakeRunGuard::lock_held_by(42).acquire(),
        Err(FatalError::LockHeld { pid: 42 })
    );
}

#[test]
fn fake_clock_should_be_settable_without_sleeping() {
    let c = FakeClock::at(100);
    c.advance(5);
    assert_eq!(c.now_epoch(), 105);
    c.set(7);
    assert_eq!(c.now_epoch(), 7);
}

#[test]
fn fake_fs_should_inject_errno_and_list_children() {
    let mut fs = FakeFs::new();
    fs.add_dir("/r")
        .add_file("/r/a", "hello")
        .add_symlink("/r/l")
        .fail("/r/bad", 13);
    fs.add_file("/r/bad", "x");
    let names: Vec<_> = fs
        .read_dir(Path::new("/r"))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names.len(), 3);
    assert_eq!(fs.lstat(Path::new("/r/a")).unwrap().size, 5);
    assert_eq!(fs.read_to_string(Path::new("/r/a")).unwrap(), "hello");
    assert_eq!(
        fs.lstat(Path::new("/r/bad")).unwrap_err().raw_os_error(),
        Some(13)
    );
    assert_eq!(
        fs.lstat(Path::new("/r/none")).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn fake_space_meter_should_script_snapshots_and_log_order_across_fakes() {
    let log = EventLog::new();
    let m = FakeSpaceMeter::new(
        vec![
            FakeSpaceMeter::plain_snapshot("/", 10, 100),
            FakeSpaceMeter::plain_snapshot("/", 4, 100),
        ],
        FakeSpaceMeter::stable(),
    )
    .with_log(&log);
    let root = Path::new("/");
    assert_eq!(m.snapshot(root).used_bytes, 10);
    assert!(m.settle(root, Duration::from_secs(1)).stable);
    assert_eq!(m.snapshot(root).used_bytes, 4);
    assert_eq!(m.snapshot(root).used_bytes, 4);
    assert_eq!(log.events(), ["snapshot", "settle", "snapshot", "snapshot"]);
    assert_eq!(m.calls()[1], MeterCall::Settle("/".into()));
}

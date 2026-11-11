#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    non_snake_case
)]
//! The `btrfs` balance target (Story 4.3.2, Task 4.3.2b) over the `Executor` with fake ports. No real
//! `btrfs`, `systemctl` or `sudo` runs here: the runner is a `FakeCommandRunner` and the numbers are
//! synthetic snapshots. The owner's real filesystem is never touched.

use std::path::{Path, PathBuf};
use std::time::Duration;

use disk_cleanup_adapters::targets::btrfs_balance::{balance_mount, BtrfsBalanceTarget};
use disk_cleanup_core::audit_types::{AuditRecord, RunRoots};
use disk_cleanup_core::executor::{
    AbortCause, Executor, ExitClass, Mode, RunMeta, RunOptions, RunReport,
};
use disk_cleanup_core::ports::{CmdOutput, Env};
use disk_cleanup_core::selector::{KindSelector, Selection};
use disk_cleanup_core::target::{Guarded, KindGate, Target};
use disk_cleanup_core::targets::btrfs_guard::BALANCE_STILL_RUNNING;
use disk_cleanup_core::testing::{
    CallKind, FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs, FakePlanStore, FakeProbe,
    FakeRunGuard, FakeSpaceMeter,
};
use disk_cleanup_core::{
    AllocSnapshot, ApplyToken, BuildInfo, DeleteError, Dirs, FsSnapshot, MountEntry, Platform,
    SelectorId, SkipReason, SpaceUse, Status,
};

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
const HOME: &str = "/home/u";

fn snap(unallocated: u64, metadata_headroom: u64) -> FsSnapshot {
    FsSnapshot {
        path: PathBuf::from("/"),
        taken_at: 0,
        total_bytes: 500 * GIB,
        used_bytes: 400 * GIB,
        avail_bytes: 100 * GIB,
        fs_type: "btrfs".into(),
        btrfs: Some(AllocSnapshot {
            data: SpaceUse {
                bytes_used: 300 * GIB,
                total_bytes: 400 * GIB,
            },
            metadata: SpaceUse {
                bytes_used: 20 * GIB,
                total_bytes: 20 * GIB + metadata_headroom,
            },
            system: SpaceUse {
                bytes_used: MIB,
                total_bytes: 8 * MIB,
            },
            unallocated,
        }),
    }
}

struct Rig {
    fs: FakeFs,
    runner: FakeCommandRunner,
    clock: FakeClock,
    probe: FakeProbe,
    space: FakeSpaceMeter,
    audit: FakeAuditSink,
    guard: FakeRunGuard,
    plans: FakePlanStore,
}

impl Rig {
    /// `reads` are the unallocated values the target reads in order (the start, then one per step). The
    /// executor itself takes three snapshots before the target does (before, discovery, second sample), so
    /// three copies of `reads[0]` come first.
    fn new(euid: u32, reads: &[u64]) -> Rig {
        let mut snaps = vec![snap(reads[0], 3 * GIB); 3];
        snaps.extend(reads.iter().map(|u| snap(*u, 3 * GIB)));
        Rig::with_snapshots(euid, snaps)
    }

    fn with_snapshots(euid: u32, snaps: Vec<FsSnapshot>) -> Rig {
        Rig::with_status(euid, snaps, Idle::default())
    }

    fn with_status(euid: u32, snaps: Vec<FsSnapshot>, status: Idle) -> Rig {
        let runner = FakeCommandRunner::new();
        runner.on(&["btrfs", "balance", "status", "/"], status.balance);
        runner.on(&["btrfs", "scrub", "status", "/"], status.scrub);
        runner.on(
            &["systemctl", "is-active", "btrfs-balance.timer"],
            status.timer,
        );
        Rig {
            fs: FakeFs::new(),
            runner,
            clock: FakeClock::at(1_800_000_000),
            probe: FakeProbe::new().with_euid(euid).with_mounts(vec![
                mount("/", 26),
                mount("/home", 26),
                mount("/boot", 3),
            ]),
            space: FakeSpaceMeter::new(snaps, FakeSpaceMeter::stable()),
            audit: FakeAuditSink::new(),
            guard: FakeRunGuard::new(),
            plans: FakePlanStore::new(),
        }
    }

    fn env(&self) -> Env<'_> {
        Env {
            fs: &self.fs,
            runner: &self.runner,
            clock: &self.clock,
            probe: &self.probe,
            space: &self.space,
            audit: &self.audit,
            guard: &self.guard,
            plans: &self.plans,
            dirs: Dirs {
                home: PathBuf::from(HOME),
                state: PathBuf::from("/home/u/.local/state"),
                cache: PathBuf::from("/home/u/.cache"),
                config: PathBuf::from("/home/u/.config"),
            },
        }
    }

    fn mutating_argvs(&self) -> Vec<String> {
        self.runner
            .calls_of(CallKind::RunMutating)
            .iter()
            .map(disk_cleanup_core::testing::RecordedCall::rendered)
            .collect()
    }
}

type Reply = Result<CmdOutput, DeleteError>;

/// The three read-only answers the guard reads; the default is an idle host.
struct Idle {
    balance: Reply,
    scrub: Reply,
    timer: Reply,
}

impl Default for Idle {
    fn default() -> Idle {
        Idle {
            balance: Ok(out("No balance found on '/'\n")),
            scrub: Ok(out("UUID: x\n\tno stats available\n")),
            timer: Err(DeleteError::CommandFailed {
                status: 3,
                message: "inactive".into(),
            }),
        }
    }
}

fn three_gib() -> Vec<FsSnapshot> {
    vec![snap(3 * GIB, 3 * GIB); 8]
}

fn mount(p: &str, dev: u64) -> MountEntry {
    MountEntry {
        mount_point: PathBuf::from(p),
        dev,
        mnt_id: dev,
    }
}

fn out(s: &str) -> CmdOutput {
    CmdOutput {
        stdout: s.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

fn relocated(n: u32) -> CmdOutput {
    out(&format!("Done, had to relocate {n} out of 120 chunks\n"))
}

/// Registers the five ladder commands, each answering `relocated(n)`.
fn on_steps(rig: &Rig, moved: [u32; 5]) {
    let argvs = [
        "-dusage=0",
        "-dusage=5,limit=8",
        "-dusage=10,limit=8",
        "-dusage=20,limit=8",
        "-dusage=40,limit=8",
    ];
    for (a, n) in argvs.iter().zip(moved) {
        rig.runner
            .on(&["btrfs", "balance", "start", a, "/"], Ok(relocated(n)));
    }
}

fn sel() -> Selection {
    Selection::new(
        KindSelector::of(&[SelectorId::Btrfs]),
        KindSelector::default(),
    )
}

fn options() -> RunOptions {
    RunOptions {
        jobs: 4,
        settle_timeout: Duration::from_secs(1),
        meter_path: None,
        meta: RunMeta {
            run_id: "run-1".into(),
            build: BuildInfo {
                bin_version: "0.1.0".into(),
                git_commit: "abc".into(),
                dirty: false,
                metascope_rev: "none".into(),
                patched: false,
                metascope_dirty: false,
            },
            stamp: None,
            break_glass: None,
            roots: RunRoots::default(),
            verdict_start: None,
        },
    }
}

fn apply(rig: &Rig, target: &BtrfsBalanceTarget) -> RunReport {
    let env = rig.env();
    Executor::new(&env, vec![target as &dyn Target], options())
        .run(&sel(), &Mode::Apply(ApplyToken::for_tests()), None)
        .unwrap_or_else(|r| panic!("apply refused: {}", r.text))
}

fn only_outcome(r: &RunReport) -> &disk_cleanup_core::Outcome {
    assert_eq!(r.summary.outcomes.len(), 1, "{:?}", r.summary.outcomes);
    &r.summary.outcomes[0]
}

// ------------------------------------------------------------------------------------------ the ladder

#[test]
fn ladder_should_run_every_step_with_a_reread_after_each_When_3gib_unallocated() {
    let rig = Rig::new(0, &[3 * GIB, 4 * GIB, 5 * GIB, 6 * GIB, 7 * GIB, 8 * GIB]);
    on_steps(&rig, [2, 3, 1, 2, 1]);

    let r = apply(&rig, &BtrfsBalanceTarget::new());

    assert_eq!(
        rig.mutating_argvs(),
        [
            "btrfs balance start -dusage=0 /",
            "btrfs balance start -dusage=5,limit=8 /",
            "btrfs balance start -dusage=10,limit=8 /",
            "btrfs balance start -dusage=20,limit=8 /",
            "btrfs balance start -dusage=40,limit=8 /",
        ]
    );
    assert_eq!(only_outcome(&r).status, Status::Removed);
    assert_eq!(r.exit, ExitClass::Ok);
    let text = r.notes.join("\n");
    assert!(
        text.contains("-dusage=5 relocated 3 chunks; unallocated now 5.0 GiB"),
        "{text}"
    );
    assert!(text.contains("3.0 GiB before, 8.0 GiB after"), "{text}");
    assert!(
        text.contains("every step -dusage=0,5,10,20,40 ran"),
        "{text}"
    );
}

#[test]
fn ladder_should_stop_after_the_step_that_relocates_zero_chunks() {
    let rig = Rig::new(0, &[3 * GIB, 3 * GIB, 3 * GIB, 3 * GIB]);
    on_steps(&rig, [1, 1, 0, 9, 9]);

    let r = apply(&rig, &BtrfsBalanceTarget::new());

    assert_eq!(
        rig.mutating_argvs(),
        [
            "btrfs balance start -dusage=0 /",
            "btrfs balance start -dusage=5,limit=8 /",
            "btrfs balance start -dusage=10,limit=8 /",
        ]
    );
    assert!(r.notes.join("\n").contains("-dusage=10 relocated 0 chunks"));
}

#[test]
fn ladder_should_stop_at_the_target_from_the_reread_value() {
    let rig = Rig::new(0, &[3 * GIB, 4 * GIB, 11 * GIB]);
    on_steps(&rig, [1, 1, 1, 1, 1]);

    let r = apply(&rig, &BtrfsBalanceTarget::new());

    assert_eq!(rig.mutating_argvs().len(), 2);
    assert!(r.notes.join("\n").contains("reached the 10.0 GiB target"));
}

#[test]
fn every_non_zero_step_cmd_should_carry_dlimit_8_When_issued() {
    let rig = Rig::new(0, &[3 * GIB, 4 * GIB, 5 * GIB, 6 * GIB, 7 * GIB, 8 * GIB]);
    on_steps(&rig, [1, 1, 1, 1, 1]);

    apply(&rig, &BtrfsBalanceTarget::new());

    let argvs = rig.mutating_argvs();
    assert_eq!(argvs.len(), 5);
    for a in &argvs[1..] {
        assert!(a.contains(",limit=8 "), "{a}");
    }
    assert!(
        !argvs[0].contains("limit"),
        "the empty-chunk step has no limit: {}",
        argvs[0]
    );
    assert!(argvs.iter().all(|a| !a.contains("sudo")));
}

#[test]
fn only_dusage_0_should_run_and_the_report_should_say_why_When_unallocated_400mib() {
    let rig = Rig::new(0, &[400 * MIB, 400 * MIB]);
    on_steps(&rig, [3, 3, 3, 3, 3]);

    let r = apply(&rig, &BtrfsBalanceTarget::new());

    assert_eq!(rig.mutating_argvs(), ["btrfs balance start -dusage=0 /"]);
    let text = r.notes.join("\n");
    assert!(text.contains("-dusage=5 and larger did not run"), "{text}");
    assert!(text.contains("unallocated is 400.0 MiB"), "{text}");
    assert!(text.contains("below the 1.0 GiB"), "{text}");
}

#[test]
fn the_owner_machine_state_should_get_dusage_0_alone_When_1mib_unallocated_and_metadata_headroom() {
    // unallocated 1 MiB, 3.3 GiB of metadata headroom: the preflight proceeds (headroom above 64 MiB)
    let rig = Rig::new(0, &[MIB, MIB]);
    on_steps(&rig, [0, 5, 5, 5, 5]);

    let r = apply(&rig, &BtrfsBalanceTarget::new());

    assert_eq!(rig.mutating_argvs(), ["btrfs balance start -dusage=0 /"]);
    assert_eq!(r.exit, ExitClass::Ok);
}

#[test]
fn a_starved_filesystem_should_warn_not_refuse_and_run_dusage_0_alone_When_only_btrfs_is_selected()
{
    // unallocated 1 MiB and metadata headroom 8 MiB: starved, so any other kind would be refused
    let starved = |u: u64| snap(u, 8 * MIB);
    let rig = Rig::with_snapshots(0, vec![starved(MIB); 8]);
    on_steps(&rig, [0, 5, 5, 5, 5]);

    let r = apply(&rig, &BtrfsBalanceTarget::new());

    assert_eq!(rig.mutating_argvs(), ["btrfs balance start -dusage=0 /"]);
    assert_eq!(r.exit, ExitClass::Ok);
    assert!(r.fatal.is_none());
}

#[test]
fn a_step_without_a_relocated_count_should_end_the_ladder_without_inventing_one() {
    let rig = Rig::new(0, &[3 * GIB, 3 * GIB, 3 * GIB]);
    rig.runner.on(
        &["btrfs", "balance", "start", "-dusage=0", "/"],
        Ok(relocated(2)),
    );
    rig.runner.on(
        &["btrfs", "balance", "start", "-dusage=5,limit=8", "/"],
        Ok(out("something else\n")),
    );

    let r = apply(&rig, &BtrfsBalanceTarget::new());

    assert_eq!(rig.mutating_argvs().len(), 2);
    assert!(r
        .notes
        .join("\n")
        .contains("did not say how many chunks moved"));
}

// ----------------------------------------------------------------------------------------- the guards

#[test]
fn balance_should_Skip_AlreadyRunning_naming_balance_scrub_or_timer_When_active() {
    let cases = [
        (
            Idle {
                balance: Ok(out("Balance on '/' is running\n1 out of about 5 chunks balanced (2 considered),  80% left\n")),
                ..Idle::default()
            },
            "balance is already running",
        ),
        (
            Idle {
                scrub: Ok(out("UUID: x\nStatus:           running\n")),
                ..Idle::default()
            },
            "scrub is running",
        ),
        (
            Idle {
                timer: Ok(out("active\n")),
                ..Idle::default()
            },
            "btrfs-balance.timer is active",
        ),
    ];
    for (status, want) in cases {
        let rig = Rig::with_status(0, three_gib(), status);
        let target = BtrfsBalanceTarget::new();
        let env = rig.env();
        let cands = target.discover(&env, &sel());
        assert_eq!(cands.len(), 1);
        assert_eq!(
            target.guard(&cands[0], &env),
            Guarded::Skip(SkipReason::AlreadyRunning)
        );
        let detail = target.detail(&cands[0]).unwrap();
        assert!(detail.contains(want), "{detail}");
        assert_eq!(rig.runner.mutating_calls(), 0);
    }
}

#[test]
fn an_apply_should_start_nothing_When_a_balance_is_already_running() {
    let rig = Rig::with_status(
        0,
        three_gib(),
        Idle {
            balance: Ok(out("Balance on '/' is paused\n")),
            ..Idle::default()
        },
    );

    let r = apply(&rig, &BtrfsBalanceTarget::new());

    assert_eq!(rig.mutating_argvs().len(), 0);
    assert_eq!(
        only_outcome(&r).status,
        Status::Skipped(SkipReason::AlreadyRunning)
    );
}

#[test]
fn an_unreadable_balance_status_should_block_the_start() {
    let rig = Rig::with_status(
        0,
        three_gib(),
        Idle {
            balance: Err(DeleteError::CommandFailed {
                status: 1,
                message: "ERROR: cannot access '/'".into(),
            }),
            ..Idle::default()
        },
    );
    let target = BtrfsBalanceTarget::new();
    let env = rig.env();
    let c = target.discover(&env, &sel());
    assert_eq!(
        target.guard(&c[0], &env),
        Guarded::Skip(SkipReason::AlreadyRunning)
    );
}

#[test]
fn non_root_should_Skip_NeedsRoot_and_run_no_command_at_all_When_euid_1000() {
    let rig = Rig::new(1000, &[3 * GIB]);
    let target = BtrfsBalanceTarget::new();
    let env = rig.env();

    let cands = target.discover(&env, &sel());
    assert_eq!(
        target.guard(&cands[0], &env),
        Guarded::Skip(SkipReason::NeedsRoot)
    );
    let r = apply(&rig, &target);

    assert_eq!(
        only_outcome(&r).status,
        Status::Skipped(SkipReason::NeedsRoot)
    );
    assert!(
        rig.runner.calls().is_empty(),
        "a non-root run issues no command, so never `sudo`: {:?}",
        rig.runner.calls()
    );
}

// ---------------------------------------------------------------------------------- failure and signals

#[test]
fn enospc_in_a_step_should_abort_ladder_exit_2_and_print_state_When_step_fails() {
    for failure in [
        DeleteError::CommandFailed {
            status: 1,
            message: "ERROR: error during balancing '/': No space left on device".into(),
        },
        DeleteError::NoSpace,
        DeleteError::Io(28),
    ] {
        let rig = Rig::new(0, &[3 * GIB, 3 * GIB + 100 * MIB, 3 * GIB + 100 * MIB]);
        for a in ["-dusage=0", "-dusage=5,limit=8"] {
            rig.runner
                .on(&["btrfs", "balance", "start", a, "/"], Ok(relocated(1)));
        }
        rig.runner.on(
            &["btrfs", "balance", "start", "-dusage=10,limit=8", "/"],
            Err(failure),
        );

        let r = apply(&rig, &BtrfsBalanceTarget::new());

        assert_eq!(rig.mutating_argvs().len(), 3, "20 and 40 must not start");
        assert_eq!(r.exit.code(), 2);
        assert_eq!(r.abort, Some(AbortCause::NoSpace));
        let text = r.notes.join("\n");
        assert!(text.contains("-dusage=10 failed with ENOSPC"), "{text}");
        assert!(
            text.contains("Unallocated was 3.0 GiB, now 3.1 GiB"),
            "{text}"
        );
        assert!(text.contains("btrfs filesystem usage /"), "{text}");
    }
}

#[test]
fn another_failed_step_should_end_the_ladder_as_an_error_with_exit_1() {
    let rig = Rig::new(0, &[3 * GIB, 3 * GIB]);
    rig.runner.on(
        &["btrfs", "balance", "start", "-dusage=0", "/"],
        Err(DeleteError::CommandFailed {
            status: 1,
            message: "ERROR: not permitted".into(),
        }),
    );
    let r = apply(&rig, &BtrfsBalanceTarget::new());
    assert_eq!(r.exit.code(), 1);
    assert!(matches!(only_outcome(&r).status, Status::Error(_)));
    assert_eq!(rig.mutating_argvs().len(), 1);
}

/// Runs an apply whose `-dusage=10` step blocks in the kernel until a signal; the signal arrives once the
/// blocked command is recorded.
fn interrupted_run(signal: i32) -> (Rig, RunReport) {
    let rig = Rig::new(0, &[3 * GIB, 4 * GIB, 5 * GIB, 6 * GIB]);
    on_steps(&rig, [1, 1, 1, 1, 1]);
    rig.runner
        .block_until_aborted(&["btrfs", "balance", "start", "-dusage=10,limit=8", "/"]);
    let target = BtrfsBalanceTarget::new();
    let report = std::thread::scope(|s| {
        let h = s.spawn(|| apply(&rig, &target));
        while !rig
            .mutating_argvs()
            .iter()
            .any(|a| a.contains("-dusage=10"))
        {
            std::thread::yield_now();
        }
        rig.guard.interrupt(signal);
        h.join().unwrap()
    });
    (rig, report)
}

#[test]
fn sigint_during_balance_should_stop_waiting_print_the_cancel_command_exit_130_and_never_issue_balance_cancel(
) {
    let (rig, r) = interrupted_run(2);

    // no step after the blocked one, and nothing that cancels
    let all: Vec<String> = rig
        .runner
        .calls()
        .iter()
        .map(disk_cleanup_core::testing::RecordedCall::rendered)
        .collect();
    assert_eq!(
        rig.mutating_argvs().last().unwrap(),
        "btrfs balance start -dusage=10,limit=8 /"
    );
    assert!(all.iter().all(|a| !a.contains("cancel")), "{all:?}");
    assert!(all
        .iter()
        .all(|a| !a.contains("-dusage=20") && !a.contains("-dusage=40")));

    assert_eq!(only_outcome(&r).status, Status::Running);
    assert_eq!(r.exit.code(), 130);
    assert!(
        r.notes.contains(&BALANCE_STILL_RUNNING.to_string()),
        "{:?}",
        r.notes
    );
    assert_eq!(
        BALANCE_STILL_RUNNING,
        "INTERRUPTED: a btrfs balance is still running in the kernel. This tool does not stop it. Check it with 'sudo btrfs balance status /'; stop it yourself with 'sudo btrfs balance cancel /'."
    );

    // the audit result of the step says running and run_end says interrupted
    let records = rig.audit.records();
    let result = records
        .iter()
        .find_map(|r| match r {
            AuditRecord::Result(i) => Some(i.0.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(result.status, Status::Running);
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["status"], "running");
    let end = records
        .iter()
        .find_map(|r| match r {
            AuditRecord::RunEnd(e) => Some(e.clone()),
            _ => None,
        })
        .unwrap();
    assert!(end.interrupted);
}

#[test]
fn sigterm_during_balance_should_exit_143() {
    let (_, r) = interrupted_run(15);
    assert_eq!(r.exit.code(), 143);
}

#[test]
fn a_normal_run_should_not_be_marked_interrupted() {
    let rig = Rig::new(0, &[3 * GIB, 3 * GIB]);
    on_steps(&rig, [0, 0, 0, 0, 0]);
    apply(&rig, &BtrfsBalanceTarget::new());
    let end = rig
        .audit
        .records()
        .into_iter()
        .find_map(|r| match r {
            AuditRecord::RunEnd(e) => Some(e),
            _ => None,
        })
        .unwrap();
    assert!(!end.interrupted);
}

// -------------------------------------------------------------------------------- gate and discovery

#[test]
fn the_kind_should_be_unavailable_on_macos_and_without_the_btrfs_tool() {
    let mut rig = Rig::new(0, &[3 * GIB]);
    rig.probe = FakeProbe::new().with_euid(0).with_platform(Platform::Macos);
    let t = BtrfsBalanceTarget::new();
    assert_eq!(
        t.gate(&rig.env()),
        KindGate::Unavailable(SkipReason::UnsupportedOnThisOS)
    );

    let rig = Rig::new(0, &[3 * GIB]);
    rig.runner.absent("btrfs");
    assert_eq!(
        t.gate(&rig.env()),
        KindGate::Unavailable(SkipReason::ToolMissing)
    );
    assert_eq!(t.gate(&Rig::new(0, &[3 * GIB]).env()), KindGate::Available);
}

#[test]
fn discover_should_offer_nothing_When_home_is_not_on_btrfs() {
    let mut s = snap(3 * GIB, 3 * GIB);
    s.fs_type = "ext4".into();
    s.btrfs = None;
    let rig = Rig::with_snapshots(0, vec![s]);
    let t = BtrfsBalanceTarget::new();
    assert!(t.discover(&rig.env(), &sel()).is_empty());
}

#[test]
fn balance_mount_should_pick_the_shortest_mount_of_the_same_filesystem() {
    let mounts = [
        mount("/", 26),
        mount("/home", 26),
        mount("/var/log", 26),
        mount("/boot", 3),
    ];
    assert_eq!(balance_mount(&mounts, Path::new("/home/u")), Path::new("/"));
    assert_eq!(
        balance_mount(&mounts, Path::new("/boot/efi")),
        Path::new("/boot")
    );
    assert_eq!(
        balance_mount(&[], Path::new("/home/u")),
        Path::new("/home/u")
    );
    // home outside every listed mount: the path itself
    assert_eq!(
        balance_mount(&[mount("/mnt", 1)], Path::new("/home/u")),
        Path::new("/home/u")
    );
}

// ------------------------------------------------------------------------------- safety guarantees

#[test]
fn safety_btrfs_balance_is_never_cancelled_by_the_tool() {
    let (rig, _) = interrupted_run(2);
    let all: Vec<String> = rig
        .runner
        .calls()
        .iter()
        .map(disk_cleanup_core::testing::RecordedCall::rendered)
        .collect();
    assert!(all.iter().all(|a| !a.contains("cancel")), "{all:?}");
    // and `balance cancel` is not a runnable argv in either class
    for mutating in [true, false] {
        let mut cmd =
            disk_cleanup_core::ports::Cmd::new("btrfs", ["balance", "cancel", "/"], mutating);
        cmd.mutating = mutating;
        assert_eq!(disk_cleanup_core::ports::classify(&cmd), None);
    }
}

#[test]
fn safety_btrfs_balance_non_root_runs_no_command_and_never_sudo() {
    let rig = Rig::new(1000, &[3 * GIB]);
    let r = apply(&rig, &BtrfsBalanceTarget::new());
    assert_eq!(
        only_outcome(&r).status,
        Status::Skipped(SkipReason::NeedsRoot)
    );
    assert!(rig.runner.calls().is_empty(), "{:?}", rig.runner.calls());
}

#[test]
fn safety_btrfs_balance_non_zero_steps_are_bounded_and_need_one_gib_unallocated() {
    let rig = Rig::new(0, &[999 * MIB, 999 * MIB]);
    on_steps(&rig, [1, 1, 1, 1, 1]);
    apply(&rig, &BtrfsBalanceTarget::new());
    assert_eq!(rig.mutating_argvs(), ["btrfs balance start -dusage=0 /"]);

    for usage in [5u8, 10, 20, 40] {
        let unbounded = disk_cleanup_core::ports::Cmd::new(
            "btrfs",
            [
                "balance".to_string(),
                "start".into(),
                format!("-dusage={usage}"),
                "/".into(),
            ],
            true,
        );
        assert_eq!(
            disk_cleanup_core::ports::classify(&unbounded),
            None,
            "usage {usage}"
        );
    }
}

#[test]
fn safety_btrfs_balance_never_starts_beside_a_running_job() {
    let rig = Rig::with_status(
        0,
        three_gib(),
        Idle {
            scrub: Ok(out("Status:           running\n")),
            ..Idle::default()
        },
    );
    let r = apply(&rig, &BtrfsBalanceTarget::new());
    assert_eq!(
        only_outcome(&r).status,
        Status::Skipped(SkipReason::AlreadyRunning)
    );
    assert_eq!(rig.runner.mutating_calls(), 0);
}

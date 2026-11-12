#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! The `btrfs-trim` target (Story 4.3.3, Task 4.3.3a) over the `Executor` with fake ports. No real `fstrim`
//! or `sudo` runs here: the runner is a `FakeCommandRunner` and the numbers are synthetic snapshots.

use std::path::PathBuf;
use std::time::Duration;

use dunnage_adapters::targets::btrfs_trim::BtrfsTrimTarget;
use dunnage_core::audit_types::RunRoots;
use dunnage_core::executor::{Executor, Mode, RunMeta, RunOptions, RunReport};
use dunnage_core::ports::CmdOutput;
use dunnage_core::selector::{KindSelector, Selection};
use dunnage_core::target::Target;
use dunnage_core::testing::{
    CallKind, FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs, FakePlanStore, FakeProbe,
    FakeRunGuard, FakeSpaceMeter,
};
use dunnage_core::{
    AllocSnapshot, ApplyToken, BuildInfo, DeleteError, Dirs, FsSnapshot, MountEntry, SelectorId,
    SkipReason, SpaceUse, Status,
};

const GIB: u64 = 1 << 30;

fn snap() -> FsSnapshot {
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
                total_bytes: 23 * GIB,
            },
            system: SpaceUse {
                bytes_used: 1 << 20,
                total_bytes: 8 << 20,
            },
            unallocated: 9 * GIB,
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
    fn new(euid: u32, fstrim_says: Result<&str, DeleteError>) -> Rig {
        let runner = FakeCommandRunner::new();
        runner.on(
            &["fstrim", "-v", "/"],
            fstrim_says.map(|s| CmdOutput {
                stdout: s.as_bytes().to_vec(),
                stderr: Vec::new(),
            }),
        );
        Rig {
            fs: FakeFs::new(),
            runner,
            clock: FakeClock::at(1_800_000_000),
            probe: FakeProbe::new().with_euid(euid).with_mounts(vec![
                MountEntry {
                    mount_point: PathBuf::from("/"),
                    dev: 26,
                    mnt_id: 26,
                },
                MountEntry {
                    mount_point: PathBuf::from("/home"),
                    dev: 26,
                    mnt_id: 27,
                },
            ]),
            space: FakeSpaceMeter::new(vec![snap(); 8], FakeSpaceMeter::stable()),
            audit: FakeAuditSink::new(),
            guard: FakeRunGuard::new(),
            plans: FakePlanStore::new(),
        }
    }

    fn run(&self, mode: &Mode) -> RunReport {
        let env = dunnage_core::ports::Env {
            fs: &self.fs,
            runner: &self.runner,
            clock: &self.clock,
            probe: &self.probe,
            space: &self.space,
            audit: &self.audit,
            guard: &self.guard,
            plans: &self.plans,
            dirs: Dirs {
                home: PathBuf::from("/home/u"),
                state: PathBuf::from("/home/u/.local/state"),
                cache: PathBuf::from("/home/u/.cache"),
                config: PathBuf::from("/home/u/.config"),
            },
        };
        let target = BtrfsTrimTarget::new();
        let sel = Selection::new(
            KindSelector::of(&[SelectorId::BtrfsTrim]),
            KindSelector::default(),
        );
        let options = RunOptions {
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
        };
        Executor::new(&env, vec![&target as &dyn Target], options)
            .run(&sel, mode, None)
            .unwrap_or_else(|r| panic!("run refused: {}", r.text))
    }

    fn apply(&self) -> RunReport {
        self.run(&Mode::Apply(ApplyToken::for_tests()))
    }

    fn mutating_argvs(&self) -> Vec<String> {
        self.runner
            .calls_of(CallKind::RunMutating)
            .iter()
            .map(dunnage_core::testing::RecordedCall::rendered)
            .collect()
    }
}

#[test]
fn trim_should_record_trimmed_bytes_and_not_freed_When_fstrim_reports_120_gib() {
    let rig = Rig::new(
        0,
        Ok("/: 120 GiB (128849018880 bytes) trimmed on /dev/nvme0n1p2\n"),
    );

    let r = rig.apply();

    assert_eq!(rig.mutating_argvs(), ["fstrim -v /"]);
    let o = &r.summary.outcomes[0];
    assert_eq!(o.status, Status::Removed);
    assert_eq!(o.trimmed_bytes, Some(120 * GIB));
    assert_eq!(o.freed.attributed_bytes.0, 0, "a trim frees nothing");
    assert_eq!(o.measured_bytes, Some(0));
    assert_eq!(r.summary.attributed_bytes, 0);
    assert_eq!(r.summary.trimmed_bytes, 120 * GIB);
    assert_eq!(
        r.summary.trimmed_line.as_deref(),
        Some("trimmed 120.0 GiB (fstrim; discarded blocks, not freed space)")
    );
    assert!(r.notes.join("\n").contains("does not change 'unallocated'"));
}

#[test]
fn trim_should_read_the_short_form_When_fstrim_prints_trimmed_120_gib() {
    let rig = Rig::new(0, Ok("trimmed 120 GiB\n"));
    let r = rig.apply();
    assert_eq!(r.summary.outcomes[0].trimmed_bytes, Some(120 * GIB));
}

#[test]
fn trim_should_say_the_figure_is_unknown_When_fstrim_prints_nothing_recognisable() {
    let rig = Rig::new(0, Ok("done\n"));
    let r = rig.apply();
    assert_eq!(r.summary.outcomes[0].status, Status::Removed);
    assert_eq!(r.summary.outcomes[0].trimmed_bytes, None);
    assert!(r.summary.trimmed_line.is_none());
    assert!(r.notes.join("\n").contains("did not report how much"));
}

#[test]
fn trim_should_end_as_an_error_with_fstrims_text_When_discard_is_unsupported() {
    let rig = Rig::new(
        0,
        Err(DeleteError::CommandFailed {
            status: 1,
            message: "fstrim: /: the discard operation is not supported".into(),
        }),
    );
    let r = rig.apply();
    let o = &r.summary.outcomes[0];
    assert!(
        matches!(&o.status, Status::Error(m) if m.contains("discard operation is not supported")),
        "{:?}",
        o.status
    );
    assert_eq!(o.trimmed_bytes, None);
}

#[test]
fn trim_should_run_no_command_When_planning() {
    let rig = Rig::new(0, Ok("trimmed 1 GiB\n"));
    let r = rig.run(&Mode::Plan);
    assert!(rig.mutating_argvs().is_empty());
    assert_eq!(r.summary.trimmed_bytes, 0);
}

#[test]
fn safety_btrfs_trim_non_root_runs_no_command_and_never_sudo() {
    let rig = Rig::new(1000, Ok("trimmed 1 GiB\n"));
    let r = rig.apply();
    assert_eq!(
        r.summary.outcomes[0].status,
        Status::Skipped(SkipReason::NeedsRoot)
    );
    assert!(rig.runner.calls().is_empty(), "{:?}", rig.runner.calls());
}

#[test]
fn safety_btrfs_trim_runs_only_fstrim_v_on_the_mount() {
    use dunnage_core::ports::{classify, Cmd};
    let rig = Rig::new(0, Ok("trimmed 1 GiB\n"));
    rig.apply();
    assert_eq!(rig.mutating_argvs(), ["fstrim -v /"]);
    for argv in [vec!["-a"], vec!["/"], vec!["-v"], vec!["-v", "-a", "/"]] {
        assert_eq!(classify(&Cmd::new("fstrim", argv, true)), None);
    }
}

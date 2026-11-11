#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Path caches removed through `delete_tree` (Story 2.3.1). Real fixtures in tempdirs only; the target is
//! driven directly and through the `Executor` with fake ports.

mod common;
use common::discovery::*;

use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use disk_cleanup_adapters::targets::cache::{removal_via, CacheTarget, RemovalVia};
use disk_cleanup_adapters::testing::{FaultDirOps, Op};
use disk_cleanup_adapters::RealDirOps;
use disk_cleanup_core::audit_types::RunRoots;
use disk_cleanup_core::executor::{Executor, Mode, RunMeta, RunOptions};
use disk_cleanup_core::ports::{CmdOutput, Env, RunGuard};
use disk_cleanup_core::selector::{KindSelector, Selection};
use disk_cleanup_core::target::{Guarded, Target};
use disk_cleanup_core::testing::{
    FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs, FakePlanStore, FakeProbe, FakeRunGuard,
    FakeSpaceMeter,
};
use disk_cleanup_core::{
    ApplyToken, AuditRecord, BuildInfo, Candidate, DeleteError, KindFacts, SelectorId, SkipReason,
    Status,
};

const MIB: usize = 1 << 20;

struct Rig {
    fs: FakeFs,
    runner: FakeCommandRunner,
    clock: FakeClock,
    probe: FakeProbe,
    space: FakeSpaceMeter,
    audit: FakeAuditSink,
    guard: FakeRunGuard,
    plans: FakePlanStore,
    home: PathBuf,
}

impl Rig {
    fn new(f: &Fixture) -> Rig {
        Rig::with_pip(f, None)
    }

    fn with_pip(f: &Fixture, pip: Option<&Path>) -> Rig {
        let runner = FakeCommandRunner::new();
        runner.absent("go").absent("pip3");
        match pip {
            Some(p) => {
                runner.on(
                    &["pip", "cache", "dir"],
                    Ok(CmdOutput {
                        stdout: format!("{}\n", p.display()).into_bytes(),
                        stderr: Vec::new(),
                    }),
                );
            }
            None => {
                runner.absent("pip");
            }
        }
        Rig {
            fs: FakeFs::new(),
            runner,
            clock: FakeClock::at(1_700_000_000),
            probe: FakeProbe::new(),
            space: FakeSpaceMeter::new(
                vec![FakeSpaceMeter::plain_snapshot("/", 500, 1000)],
                FakeSpaceMeter::stable(),
            ),
            audit: FakeAuditSink::new(),
            guard: FakeRunGuard::new(),
            plans: FakePlanStore::new(),
            home: f.home.clone(),
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
            dirs: dirs_of(&self.home),
        }
    }
}

fn caches() -> Selection {
    Selection::new(
        KindSelector::of(&[SelectorId::Caches]),
        KindSelector::default(),
    )
}

fn target(f: &Fixture) -> CacheTarget<RealDirOps> {
    CacheTarget::new(
        roots_for(f),
        f.home.join("fsroot"),
        RealDirOps::confined_to(&f.home),
    )
}

fn fill(dir: &Path, files: usize, bytes: usize) {
    fs::create_dir_all(dir).unwrap();
    for i in 0..files {
        fs::write(dir.join(format!("f{i}")), vec![7u8; bytes]).unwrap();
    }
}

fn by_label<'a>(cs: &'a [Candidate], label: &str) -> &'a Candidate {
    cs.iter()
        .find(|c| matches!(c.facts(), KindFacts::Cache(f) if f.label == label))
        .unwrap_or_else(|| panic!("no candidate {label}"))
}

fn options() -> RunOptions {
    RunOptions {
        jobs: 1,
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

#[test]
fn apply_should_remove_both_caches_and_log_two_results_When_npm_and_cargo_src_exist() {
    let f = fixture();
    fill(&f.home.join(".npm/_cacache"), 10, MIB);
    fill(&f.home.join(".cargo/registry/src"), 3, MIB);
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();

    let ex = Executor::new(&env, vec![&t as &dyn Target], options());
    let report = ex
        .run(&caches(), &Mode::Apply(ApplyToken::for_tests()), None)
        .unwrap_or_else(|r| panic!("refused: {}", r.text));

    assert!(!f.home.join(".npm/_cacache").exists());
    assert!(!f.home.join(".cargo/registry/src").exists());
    assert!(f.home.join(".npm").exists(), "only the cache itself goes");
    let mut measured: Vec<u64> = report
        .summary
        .outcomes
        .iter()
        .filter(|o| o.status == Status::Removed)
        .map(|o| o.measured_bytes.unwrap())
        .collect();
    measured.sort_unstable();
    assert_eq!(measured.len(), 2, "{:?}", report.summary.outcomes);
    let near = |got: u64, want: usize| got.abs_diff(want as u64) <= 64 * 1024;
    assert!(near(measured[0], 3 * MIB), "{measured:?}");
    assert!(near(measured[1], 10 * MIB), "{measured:?}");
    let results = rig
        .audit
        .records()
        .iter()
        .filter(|r| matches!(r, AuditRecord::Result(_)))
        .count();
    assert_eq!(results, 2);
}

#[test]
fn apply_should_remove_a_read_only_tree_and_chmod_only_inside_the_root_When_dirs_are_0555() {
    let f = fixture();
    let src = f.home.join(".cargo/registry/src");
    let deep = src.join("index-1/crate-1.0/src");
    fill(&deep, 2, 1024);
    for d in [&deep, &src.join("index-1/crate-1.0"), &src.join("index-1")] {
        fs::set_permissions(d, fs::Permissions::from_mode(0o555)).unwrap();
    }
    let parent = f.home.join(".cargo/registry");
    let mode_of = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    let parent_before = mode_of(&parent);
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();

    let cands = t.discover(&env, &caches());
    let c = by_label(&cands, "Cargo registry src");
    assert_eq!(t.guard(c, &env), Guarded::Allow);
    let freed = t
        .execute(c, &env, &ApplyToken::for_tests())
        .expect("read-only tree is removed");

    assert!(!src.exists());
    assert_eq!(
        mode_of(&parent),
        parent_before,
        "the parent was not chmod-ed"
    );
    assert!(freed.attributed_bytes.0 >= 2048);
}

#[test]
fn discover_should_skip_symlink_and_leave_targets_alone_When_a_parent_is_a_symlink() {
    let f = fixture();
    // case 1: ~/.cache is a link to a directory elsewhere inside the fixture
    let elsewhere = f.home.join("elsewhere");
    fill(&elsewhere.join("ms-playwright"), 2, 1024);
    symlink(&elsewhere, f.home.join(".cache")).unwrap();
    // case 2: ~/.cargo/registry is a link
    let real_reg = f.home.join("real-registry");
    fill(&real_reg.join("src"), 2, 1024);
    fs::create_dir_all(f.home.join(".cargo")).unwrap();
    symlink(&real_reg, f.home.join(".cargo/registry")).unwrap();
    // an unrelated cache that must still be removed
    fill(&f.home.join(".npm/_cacache"), 2, 1024);
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();

    let ex = Executor::new(&env, vec![&t as &dyn Target], options());
    let report = ex
        .run(&caches(), &Mode::Apply(ApplyToken::for_tests()), None)
        .unwrap_or_else(|r| panic!("refused: {}", r.text));

    assert!(elsewhere.join("ms-playwright/f0").exists());
    assert!(real_reg.join("src/f0").exists());
    assert!(!f.home.join(".npm/_cacache").exists());
    let status_of = |p: &Path| {
        report
            .summary
            .outcomes
            .iter()
            .find(|o| o.path.as_deref() == Some(p))
            .map(|o| o.status.clone())
    };
    let sym = Status::Skipped(SkipReason::Symlink);
    assert_eq!(
        status_of(&f.home.join(".cache/ms-playwright")),
        Some(sym.clone())
    );
    assert_eq!(status_of(&f.home.join(".cargo/registry/src")), Some(sym));
}

#[test]
fn execute_should_refuse_with_symlink_and_delete_nothing_When_a_parent_becomes_a_symlink_after_planning(
) {
    let f = fixture();
    let reg = f.home.join(".cargo/registry");
    fill(&reg.join("src"), 2, 1024);
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = t.discover(&env, &caches());
    let c = by_label(&cands, "Cargo registry src");
    assert_eq!(t.guard(c, &env), Guarded::Allow);

    let moved = f.home.join("moved-registry");
    fs::rename(&reg, &moved).unwrap();
    symlink(&moved, &reg).unwrap();

    assert_eq!(t.guard(c, &env), Guarded::Skip(SkipReason::Symlink));
    let err = t.execute(c, &env, &ApplyToken::for_tests()).unwrap_err();
    assert_eq!(err.error, DeleteError::Symlink);
    assert!(moved.join("src/f0").exists());
}

#[test]
fn execute_should_refuse_with_identity_mismatch_When_the_cache_is_replaced_after_planning() {
    let f = fixture();
    let dir = f.home.join(".npm/_cacache");
    fill(&dir, 1, 1024);
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = t.discover(&env, &caches());
    let c = by_label(&cands, "npm _cacache");

    fs::remove_dir_all(&dir).unwrap();
    fill(&dir, 1, 1024);
    // an inode number can be reused; keep the test about a replaced directory only when it differs
    if fs::metadata(&dir).unwrap().ino() == c.ident().ino {
        return;
    }

    assert_eq!(t.guard(c, &env), Guarded::Skip(SkipReason::IdentityChanged));
    let err = t.execute(c, &env, &ApplyToken::for_tests()).unwrap_err();
    assert_eq!(err.error, DeleteError::IdentityMismatch);
    assert!(dir.join("f0").exists());
}

#[test]
fn execute_should_succeed_with_nothing_freed_When_the_cache_vanished() {
    let f = fixture();
    let dir = f.home.join(".npm/_cacache");
    fill(&dir, 1, 1024);
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = t.discover(&env, &caches());
    let c = by_label(&cands, "npm _cacache");
    fs::remove_dir_all(&dir).unwrap();

    let freed = t.execute(c, &env, &ApplyToken::for_tests()).unwrap();
    assert_eq!(freed.entries_removed, 0);
}

#[test]
fn execute_should_delete_only_through_delete_tree_When_the_ops_are_counted() {
    let f = fixture();
    fill(&f.home.join(".npm/_cacache"), 3, 1024);
    let ops = FaultDirOps::wrap(RealDirOps::confined_to(&f.home)).build();
    let t = CacheTarget::new(roots_for(&f), f.home.join("fsroot"), ops);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = t.discover(&env, &caches());
    let c = by_label(&cands, "npm _cacache");

    t.execute(c, &env, &ApplyToken::for_tests()).unwrap();

    assert!(!f.home.join(".npm/_cacache").exists());
    // three files plus the directory itself, all through the engine's `unlinkat`
    assert_eq!(t.ops().calls(Op::Unlinkat), 4);
}

#[test]
fn execute_should_run_the_tool_for_a_command_row_and_never_delete_the_path_itself() {
    let f = fixture();
    let t = target(&f);
    let pip = f.home.join(".cache/pip");
    fill(&pip, 1, 1024);
    let rig = Rig::with_pip(&f, Some(&pip));
    rig.runner
        .on(&["pip", "cache", "purge"], Ok(CmdOutput::default()));
    let env = rig.env();
    let cands = t.discover(&env, &caches());
    let c = by_label(&cands, "pip cache");

    let freed = t.execute(c, &env, &ApplyToken::for_tests()).unwrap();
    assert_eq!(freed.attributed_bytes.0, 0, "the fake tool removed nothing");
    assert_eq!(
        rig.runner
            .calls_of(disk_cleanup_core::testing::CallKind::RunMutating)
            .len(),
        1
    );
    assert!(
        pip.join("f0").exists(),
        "a command row is emptied by its tool, not by delete_tree"
    );
}

#[test]
fn execute_should_refuse_When_the_abort_flag_is_set() {
    let f = fixture();
    fill(&f.home.join(".npm/_cacache"), 1, 1024);
    let t = target(&f);
    let rig = Rig::new(&f);
    let env = rig.env();
    let cands = t.discover(&env, &caches());
    let c = by_label(&cands, "npm _cacache");
    rig.guard.interrupt(2);
    assert!(rig.guard.abort_flag().is_set());

    let err = t.execute(c, &env, &ApplyToken::for_tests()).unwrap_err();
    assert_eq!(err.error, DeleteError::Refused);
    assert!(f.home.join(".npm/_cacache/f0").exists());
}

#[test]
fn removal_via_should_name_command_rows_and_path_rows() {
    let row = |label: &str, apt: bool| disk_cleanup_core::CacheFacts {
        label: label.into(),
        apt,
    };
    assert_eq!(removal_via(&row("npm _cacache", false)), RemovalVia::Path);
    assert_eq!(removal_via(&row("Gradle caches", false)), RemovalVia::Path);
    assert_eq!(
        removal_via(&row("Go build cache", false)),
        RemovalVia::Command
    );
    assert_eq!(removal_via(&row("pip cache", false)), RemovalVia::Command);
    assert_eq!(removal_via(&row("apt archives", true)), RemovalVia::Command);
}

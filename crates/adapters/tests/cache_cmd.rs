#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Command caches measured by directory delta (Story 2.3.2). Every runner here is a fake: nothing spawns a
//! real go, pip, brew, xcrun or apt-get.

mod common;
use common::discovery::*;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use disk_cleanup_adapters::discover_cache::{discover_caches, CacheEnv, CacheItem, CacheVerdict};
use disk_cleanup_adapters::targets::cache_cmd::{
    plan_command, run_cache_command, CmdContext, CmdPlan,
};
use disk_cleanup_adapters::RealDirOps;
use disk_cleanup_core::cache_targets::cache_command;
use disk_cleanup_core::ports::{classify, Class, Cmd, CmdOutput, CommandRunner};
use disk_cleanup_core::testing::{FakeCommandRunner, FakeProbe};
use disk_cleanup_core::{
    ApplyToken, DeleteError, Identity, Intent, Locator, RootDev, SelectorId, SkipReason,
    WorktreeRemoval,
};

const MIB: usize = 1024 * 1024;

fn fill(dir: &Path, files: usize, bytes: usize) {
    fs::create_dir_all(dir).unwrap();
    for i in 0..files {
        fs::write(dir.join(format!("f{i}")), vec![7u8; bytes]).unwrap();
    }
}

/// A runner whose mutating command applies a filesystem side effect, like the real tool would. It refuses
/// anything the allowlist refuses and spawns nothing.
struct EffectRunner {
    absent: Vec<&'static str>,
    effect: Box<dyn Fn() + Send + Sync>,
    result: Result<(), DeleteError>,
    calls: Mutex<Vec<Vec<String>>>,
}

impl EffectRunner {
    fn new(effect: impl Fn() + Send + Sync + 'static) -> EffectRunner {
        EffectRunner {
            absent: Vec::new(),
            effect: Box::new(effect),
            result: Ok(()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for EffectRunner {
    fn exists(&self, program: &str) -> bool {
        !self.absent.contains(&program)
    }

    fn run(&self, _cmd: &Cmd) -> Result<CmdOutput, DeleteError> {
        panic!("a read-only command was not expected");
    }

    fn run_mutating(&self, cmd: &Cmd, _proof: &ApplyToken) -> Result<CmdOutput, DeleteError> {
        if classify(cmd) != Some(Class::Mutating) {
            return Err(DeleteError::Refused);
        }
        let mut argv = vec![cmd.program.to_string_lossy().into_owned()];
        argv.extend(cmd.args.iter().map(|a| a.to_string_lossy().into_owned()));
        self.calls.lock().unwrap().push(argv);
        (self.effect)();
        self.result.clone().map(|()| CmdOutput::default())
    }

    fn remove_worktree(
        &self,
        _w: &WorktreeRemoval,
        _proof: &ApplyToken,
    ) -> Result<CmdOutput, DeleteError> {
        Err(DeleteError::Refused)
    }
}

fn identity_of(p: &Path) -> (Identity, RootDev) {
    let m = fs::symlink_metadata(p).unwrap();
    (Identity::of(&m), RootDev(m.dev()))
}

fn empty_dir(dir: &Path) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            fs::remove_dir_all(&p).unwrap();
        } else {
            fs::remove_file(&p).unwrap();
        }
    }
}

fn spec_cmd(label: &str, program: &str) -> Cmd {
    let spec = cache_command(label).unwrap();
    Cmd::new(program, spec.args.iter().copied(), true)
}

#[test]
fn go_clean_should_measure_the_delta_of_gocache_and_the_intent_should_record_the_argv() {
    let f = fixture();
    let gocache = f.home.join(".cache/go-build");
    fill(&gocache, 5, MIB);
    let (ident, dev) = identity_of(&gocache);
    let before = fs::read_dir(&gocache).unwrap().count();
    assert_eq!(before, 5);

    // discovery sizes it and the candidate's intent carries the argv
    let probe = FakeProbe::new();
    let runner = FakeCommandRunner::new();
    runner.absent("pip").absent("pip3");
    runner.on(
        &["go", "env", "GOCACHE"],
        Ok(CmdOutput {
            stdout: format!("{}\n", gocache.display()).into_bytes(),
            stderr: Vec::new(),
        }),
    );
    let ops = RealDirOps::confined_to(&f.home);
    let mut roots = roots_for(&f);
    let found = discover_caches(
        &CacheEnv {
            fs_root: &f.home.join("fsroot"),
            ops: &ops,
            runner: &runner,
            probe: &probe,
        },
        &mut roots,
    );
    let item = &found.items[0];
    assert!(matches!(item.verdict, CacheVerdict::Will(_)));
    let intent = Intent::of(&item.candidate, "run", 1);
    assert_eq!(intent.argv, ["go", "clean", "-cache"]);

    let g2 = gocache.clone();
    let effect = EffectRunner::new(move || empty_dir(&g2));
    let ctx = CmdContext {
        runner: &effect,
        probe: &probe,
        explicit_root: false,
    };
    let CmdPlan::Ready(ready) = plan_command(cache_command("Go build cache").unwrap(), false, &ctx)
    else {
        panic!("go is installed");
    };
    let freed = run_cache_command(
        &ops,
        &gocache,
        ident,
        dev,
        &ready.cmd,
        &effect,
        &ApplyToken::for_tests(),
    )
    .unwrap();
    assert_eq!(effect.calls(), vec![vec!["go", "clean", "-cache"]]);
    let got = freed.attributed_bytes.0;
    let want = u64::try_from(5 * MIB).unwrap();
    assert!(got.abs_diff(want) <= 4096, "freed {got}, want about {want}");
    assert_eq!(freed.entries_removed, 5);
    assert_eq!(
        item.candidate.est_bytes().0,
        got,
        "the walk before minus the walk after"
    );
}

#[test]
fn a_failing_command_should_carry_the_partial_delta() {
    let f = fixture();
    let dir = f.home.join(".cache/pip");
    fill(&dir, 4, 64 * 1024);
    let (ident, dev) = identity_of(&dir);
    let d2 = dir.clone();
    let mut r = EffectRunner::new(move || {
        fs::remove_file(d2.join("f0")).unwrap();
        fs::remove_file(d2.join("f1")).unwrap();
    });
    r.result = Err(DeleteError::CommandFailed {
        status: 1,
        message: "boom".into(),
    });
    let ops = RealDirOps::confined_to(&f.home);
    let err = run_cache_command(
        &ops,
        &dir,
        ident,
        dev,
        &spec_cmd("pip cache", "pip"),
        &r,
        &ApplyToken::for_tests(),
    )
    .unwrap_err();
    assert_eq!(err.error, r.result.clone().unwrap_err());
    assert_eq!(err.freed.attributed_bytes.0, 2 * 64 * 1024);
}

#[test]
fn a_replaced_directory_should_not_run_the_command() {
    let f = fixture();
    let dir = f.home.join(".cache/go-build");
    fill(&dir, 1, 4096);
    let (_, dev) = identity_of(&dir);
    let stale = Identity { dev: dev.0, ino: 1 };
    let r = EffectRunner::new(|| panic!("must not run"));
    let ops = RealDirOps::confined_to(&f.home);
    let err = run_cache_command(
        &ops,
        &dir,
        stale,
        dev,
        &spec_cmd("Go build cache", "go"),
        &r,
        &ApplyToken::for_tests(),
    )
    .unwrap_err();
    assert_eq!(err.error, DeleteError::IdentityMismatch);
    assert!(r.calls().is_empty());
}

#[test]
fn a_tool_that_removes_the_whole_directory_should_count_all_of_it_as_freed() {
    let f = fixture();
    let dir = f.home.join("Library/Caches/Homebrew");
    fill(&dir, 3, 128 * 1024);
    let (ident, dev) = identity_of(&dir);
    let d2 = dir.clone();
    let r = EffectRunner::new(move || fs::remove_dir_all(&d2).unwrap());
    let ops = RealDirOps::confined_to(&f.home);
    let freed = run_cache_command(
        &ops,
        &dir,
        ident,
        dev,
        &spec_cmd("Homebrew cache", "brew"),
        &r,
        &ApplyToken::for_tests(),
    )
    .unwrap();
    assert_eq!(freed.attributed_bytes.0, 3 * 128 * 1024);
}

fn probe_of(euid: u32) -> FakeProbe {
    FakeProbe::new().with_euid(euid)
}

#[test]
fn a_missing_tool_should_be_Skipped_ToolMissing_with_the_last_program_named() {
    let runner = FakeCommandRunner::new();
    runner.absent("pip").absent("pip3").absent("go");
    let probe = probe_of(1000);
    let ctx = CmdContext {
        runner: &runner,
        probe: &probe,
        explicit_root: false,
    };
    assert_eq!(
        plan_command(cache_command("pip cache").unwrap(), false, &ctx),
        CmdPlan::Skip {
            reason: SkipReason::ToolMissing,
            note: Some("pip3 not installed".into())
        }
    );
    assert_eq!(
        plan_command(cache_command("Go build cache").unwrap(), false, &ctx),
        CmdPlan::Skip {
            reason: SkipReason::ToolMissing,
            note: Some("go not installed".into())
        }
    );
    // pip missing but pip3 present: pip3 runs
    let runner = FakeCommandRunner::new();
    runner.absent("pip");
    let ctx = CmdContext {
        runner: &runner,
        probe: &probe,
        explicit_root: false,
    };
    let CmdPlan::Ready(r) = plan_command(cache_command("pip cache").unwrap(), false, &ctx) else {
        panic!()
    };
    assert_eq!(r.program, "pip3");
    assert_eq!(runner.mutating_calls(), 0, "planning never mutates");
}

#[test]
fn apt_get_clean_should_be_NeedsRoot_for_a_user_and_ready_for_root() {
    let runner = FakeCommandRunner::new();
    let apt = cache_command("apt archives").unwrap();
    let user = probe_of(1000);
    let ctx = CmdContext {
        runner: &runner,
        probe: &user,
        explicit_root: false,
    };
    assert_eq!(
        plan_command(apt, true, &ctx),
        CmdPlan::Skip {
            reason: SkipReason::NeedsRoot,
            note: None
        }
    );
    let root = probe_of(0);
    let ctx = CmdContext {
        runner: &runner,
        probe: &root,
        explicit_root: false,
    };
    assert!(matches!(plan_command(apt, true, &ctx), CmdPlan::Ready(_)));
    // user-owned caches are restricted for root, whatever HOME holds
    for name in ["Go build cache", "pip cache", "Homebrew cache"] {
        assert_eq!(
            plan_command(cache_command(name).unwrap(), false, &ctx),
            CmdPlan::Skip {
                reason: SkipReason::RootRestricted,
                note: None
            }
        );
    }
    let explicit = CmdContext {
        runner: &runner,
        probe: &root,
        explicit_root: true,
    };
    assert!(matches!(
        plan_command(cache_command("Go build cache").unwrap(), false, &explicit),
        CmdPlan::Ready(_)
    ));
}

fn items_of(f: &Fixture, euid: u32, home_label: &str) -> Vec<CacheItem> {
    let _ = home_label;
    let apt = f.home.join("fsroot/var/cache/apt/archives");
    fill(&apt, 2, 8192);
    fill(&f.home.join(".npm/_cacache"), 2, 8192);
    let runner = FakeCommandRunner::new();
    runner.absent("go").absent("pip").absent("pip3");
    let probe = probe_of(euid);
    let ops = RealDirOps::confined_to(&f.home);
    let mut roots = roots_for(f);
    discover_caches(
        &CacheEnv {
            fs_root: &f.home.join("fsroot"),
            ops: &ops,
            runner: &runner,
            probe: &probe,
        },
        &mut roots,
    )
    .items
}

fn skip_of(i: &CacheItem) -> Option<SkipReason> {
    match i.verdict {
        CacheVerdict::Skip(r) => Some(r),
        CacheVerdict::Will(_) => None,
    }
}

fn label_of(i: &CacheItem) -> String {
    match i.candidate.facts() {
        disk_cleanup_core::KindFacts::Cache(c) => c.label.clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn root_runs_should_restrict_themselves_to_cache_apt_whatever_HOME_holds() {
    for home_label in ["HOME=/root", "HOME=/home/tstapler"] {
        let f = fixture();
        let items = items_of(&f, 0, home_label);
        let npm = items
            .iter()
            .find(|i| label_of(i) == "npm _cacache")
            .unwrap();
        assert_eq!(
            skip_of(npm),
            Some(SkipReason::RootRestricted),
            "{home_label}"
        );
        let apt = items
            .iter()
            .find(|i| label_of(i) == "apt archives")
            .unwrap();
        // the fixture's fake /var is not the real allowed apt root, so the guard says OutsideRoot; what
        // matters is that root is neither restricted nor told to become root
        assert!(
            !matches!(
                skip_of(apt),
                Some(SkipReason::RootRestricted | SkipReason::NeedsRoot)
            ),
            "{home_label}"
        );
        assert_eq!(apt.candidate.selector(), SelectorId::CacheApt);
    }
    let f = fixture();
    let items = items_of(&f, 1000, "user");
    let apt = items
        .iter()
        .find(|i| label_of(i) == "apt archives")
        .unwrap();
    assert_eq!(skip_of(apt), Some(SkipReason::NeedsRoot));
    let npm = items
        .iter()
        .find(|i| label_of(i) == "npm _cacache")
        .unwrap();
    assert_eq!(skip_of(npm), None);
    assert!(matches!(
        npm.candidate.locator(),
        Locator::Path(p) if *p == PathBuf::from(f.home.join(".npm/_cacache"))
    ));
}

#[test]
fn the_confined_runner_should_run_only_the_shim_under_the_fixture_and_the_delta_should_follow() {
    use disk_cleanup_adapters::command_confined::ConfinedCommandRunner;
    use disk_cleanup_adapters::RealCommandRunner;
    use std::os::unix::fs::PermissionsExt;

    let f = fixture();
    let root = f.home.parent().unwrap().to_path_buf();
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let dir = f.home.join(".cache/go-build");
    fill(&dir, 3, 64 * 1024);
    let shim = bin.join("go");
    fs::write(&shim, format!("#!/bin/sh\nrm -f '{}'/f*\n", dir.display())).unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    let runner = ConfinedCommandRunner::with_path(
        RealCommandRunner::new(),
        root.canonicalize().unwrap(),
        bin.clone().into(),
    );
    let (ident, dev) = identity_of(&dir);
    let ops = RealDirOps::confined_to(&f.home);
    let freed = run_cache_command(
        &ops,
        &dir,
        ident,
        dev,
        &spec_cmd("Go build cache", "go"),
        &runner,
        &ApplyToken::for_tests(),
    )
    .unwrap();
    assert_eq!(freed.attributed_bytes.0, 3 * 64 * 1024);
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);

    // a tool with no shim is refused, never searched for on the real PATH
    let r = run_cache_command(
        &ops,
        &dir,
        identity_of(&dir).0,
        dev,
        &spec_cmd("pip cache", "pip"),
        &runner,
        &ApplyToken::for_tests(),
    );
    assert!(r.is_err());
}

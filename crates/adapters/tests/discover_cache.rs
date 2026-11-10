#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Cache discovery against generated fixtures in tempdirs (Story 1.3.2; validation T-121, T-122, T-123).

mod common;
use common::discovery::*;

use std::fs;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};

use disk_cleanup_adapters::discover_cache::{
    discover_caches, CacheDiscovery, CacheEnv, CacheVerdict, Via,
};
use disk_cleanup_adapters::testing::{FaultDirOps, Op};
use disk_cleanup_adapters::treestat::{measure, MeasureOpts};
use disk_cleanup_adapters::{open_verified_root, RealDirOps};
use disk_cleanup_core::ports::CmdOutput;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::testing::{FakeCommandRunner, FakeProbe};
use disk_cleanup_core::{Identity, Locator, RootDev, SkipReason};

#[allow(clippy::unnecessary_wraps)]
fn answer(path: &Path) -> Result<CmdOutput, disk_cleanup_core::DeleteError> {
    Ok(CmdOutput {
        stdout: format!("{}\n", path.display()).into_bytes(),
        stderr: Vec::new(),
    })
}

fn fill(dir: &Path, files: usize, bytes: usize) {
    fs::create_dir_all(dir).unwrap();
    for i in 0..files {
        fs::write(dir.join(format!("f{i}")), vec![7u8; bytes]).unwrap();
    }
}

struct Run {
    out: CacheDiscovery,
    ops_open_root: usize,
    ops_stat_at: usize,
}

fn run(f: &Fixture, runner: &FakeCommandRunner, probe: &FakeProbe, roots: &mut RootSet) -> Run {
    let ops = FaultDirOps::wrap(RealDirOps::confined_to(&f.home)).build();
    let env = CacheEnv {
        fs_root: &f.home.join("fsroot"),
        ops: &ops,
        runner,
        probe,
    };
    let out = discover_caches(&env, roots);
    Run {
        out,
        ops_open_root: ops.calls(Op::OpenRoot),
        ops_stat_at: ops.calls(Op::StatAt),
    }
}

fn walked_size(p: &Path) -> u64 {
    let ops = RealDirOps::confined_to(p);
    let m = fs::symlink_metadata(p).unwrap();
    let root = open_verified_root(&ops, p, Identity::of(&m)).unwrap();
    measure(&ops, root.dir(), RootDev(m.dev()), MeasureOpts::default())
        .allocated
        .0
}

fn path_of(i: &disk_cleanup_adapters::discover_cache::CacheItem) -> PathBuf {
    match i.candidate.locator() {
        Locator::Path(p) => p.clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn go_and_pip_caches_should_be_sized_via_command_When_fake_runner_answers_env_and_cache_dir() {
    let f = fixture();
    let go = f.home.join(".cache/go-build");
    let pip = f.home.join(".cache/pip");
    fill(&go, 6, 40_000);
    fill(&pip, 3, 20_000);
    let runner = FakeCommandRunner::new();
    runner.on(&["go", "env", "GOCACHE"], answer(&go));
    runner.on(&["pip", "cache", "dir"], answer(&pip));
    let mut roots = roots_for(&f);
    let r = run(&f, &runner, &FakeProbe::new(), &mut roots);

    assert_eq!(r.out.items.len(), 2, "{:?}", r.out.items);
    for (item, dir) in r.out.items.iter().zip([&go, &pip]) {
        assert_eq!(item.via, Via::Command);
        assert!(matches!(item.verdict, CacheVerdict::Will(_)), "{item:?}");
        assert_eq!(path_of(item), *dir);
        let want = walked_size(dir);
        assert!(want > 0);
        assert_eq!(item.candidate.est_bytes().0, want, "size from the dir walk");
    }
    assert!(r.out.missing_tools.is_empty());
}

#[test]
fn cache_root_should_be_Skip_Symlink_When_dot_cargo_is_symlink() {
    let f = fixture();
    let other = f.home.parent().unwrap().join("other-cargo");
    fill(&other.join("registry/cache"), 2, 10_000);
    symlink(&other, f.home.join(".cargo")).unwrap();
    let runner = FakeCommandRunner::new();
    runner.absent("go").absent("pip").absent("pip3");
    let mut roots = roots_for(&f);
    let r = run(&f, &runner, &FakeProbe::new(), &mut roots);

    assert_eq!(r.out.items.len(), 1, "{:?}", r.out.items);
    let item = &r.out.items[0];
    assert_eq!(path_of(item), f.home.join(".cargo/registry/cache"));
    assert!(
        matches!(item.verdict, CacheVerdict::Skip(SkipReason::Symlink)),
        "{item:?}"
    );
    assert_eq!(
        item.candidate.est_bytes().0,
        0,
        "a refused root is not walked"
    );
    assert_eq!(r.ops_open_root, 0);
    assert_eq!(r.out.missing_tools, vec!["go", "pip"]);
}

#[test]
fn tool_reported_cache_path_should_be_Skip_OutsideRoot_When_gocache_opt_shared_or_home_or_wrong_basename_or_pip_dot_cache(
) {
    let f = fixture();
    let outside = f.home.parent().unwrap().join("opt/shared/go-build");
    fill(&outside, 2, 10_000);
    fs::create_dir_all(f.home.join("Documents/x")).unwrap();
    fs::create_dir_all(f.home.join(".cache")).unwrap();
    let cases: [(PathBuf, bool); 4] = [
        (outside, true),
        (f.home.clone(), true),
        (f.home.join("Documents/x"), true),
        (f.home.join(".cache"), false),
    ];
    for (reported, is_go) in cases {
        let runner = FakeCommandRunner::new();
        if is_go {
            runner.on(&["go", "env", "GOCACHE"], answer(&reported));
            runner.absent("pip").absent("pip3");
        } else {
            runner.on(&["pip", "cache", "dir"], answer(&reported));
            runner.absent("go");
        }
        let mut roots = roots_for(&f);
        let r = run(&f, &runner, &FakeProbe::new(), &mut roots);
        let item = r
            .out
            .items
            .iter()
            .find(|i| i.via == Via::Command)
            .unwrap_or_else(|| panic!("{reported:?}: no command item in {:?}", r.out.items));
        assert!(
            matches!(item.verdict, CacheVerdict::Skip(SkipReason::OutsideRoot)),
            "{reported:?}: {item:?}"
        );
        assert_eq!(path_of(item), reported);
        assert_eq!(
            (r.ops_open_root, r.ops_stat_at),
            (0, 0),
            "{reported:?} was walked"
        );
    }
}

#[test]
fn apt_should_be_Skip_NeedsRoot_and_not_walked_When_not_root() {
    let f = fixture();
    fill(&f.home.join("fsroot/var/cache/apt/archives"), 2, 10_000);
    let runner = FakeCommandRunner::new();
    runner.absent("go").absent("pip").absent("pip3");
    let mut roots = roots_for(&f);
    let r = run(&f, &runner, &FakeProbe::new(), &mut roots);
    assert_eq!(r.out.items.len(), 1);
    let item = &r.out.items[0];
    assert!(matches!(
        item.verdict,
        CacheVerdict::Skip(SkipReason::NeedsRoot)
    ));
    assert!(matches!(
        item.candidate.facts(),
        disk_cleanup_core::KindFacts::Cache(c) if c.apt
    ));
    assert_eq!(r.ops_open_root, 0);
}

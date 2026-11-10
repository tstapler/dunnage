#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names,
    non_snake_case
)]
//! Build-dir discovery against generated fixtures in tempdirs (Story 1.3.1; validation T-009, T-010,
//! T-160, T-161, T-421).

mod common;
use common::discovery::*;

use std::fs;
use std::os::unix::fs::symlink;
use std::time::Duration;

use disk_cleanup_adapters::discover_build::{
    decide_build_dirs, discover_build_dirs, scan_options, BuildEnv, BuildKinds, BuildOpts,
    BuildVerdict, RawBuildDir,
};
use disk_cleanup_adapters::mtime_probe::{probe_newest_mtime, NewestStore, ProbeTarget, Recency};
use disk_cleanup_adapters::testing::{FaultDirOps, Op};
use disk_cleanup_adapters::RealDirOps;
use disk_cleanup_core::testing::{FakeClock, FakeProbe};
use disk_cleanup_core::{BuildDirFlavor, Identity, RootDev, SkipReason};

fn opts() -> BuildOpts {
    BuildOpts {
        min_age: HOUR,
        kinds: BuildKinds::ALL,
        max_duration: None,
        rescan: false,
        walker_cache: None,
    }
}

fn raw(path: &std::path::Path, flavor: BuildDirFlavor, lock: Option<u64>) -> RawBuildDir {
    RawBuildDir {
        path: path.to_path_buf(),
        flavor,
        group: path.parent().unwrap().to_path_buf(),
        lockfile_hash: lock,
        est_bytes: 4096,
    }
}

#[test]
fn discovery_should_return_three_candidates_and_one_cluster_When_two_identical_package_locks() {
    let f = fixture();
    node_checkout(&f.code.join("a"), Some("lock-v1"));
    node_checkout(&f.code.join("c"), Some("lock-v1"));
    fs::create_dir_all(f.code.join("b/target")).unwrap();
    fs::write(f.code.join("b/Cargo.toml"), "[package]").unwrap();
    fs::write(f.code.join("b/target/y"), "y").unwrap();

    let roots = roots_for(&f);
    let ops = RealDirOps::confined_to(&f.home);
    let (probe, clock) = (FakeProbe::new(), FakeClock::at(now_epoch()));
    let env = BuildEnv {
        ops: &ops,
        roots: &roots,
        probe: &probe,
        clock: &clock,
    };
    let d = discover_build_dirs(&env, &opts()).unwrap();

    assert_eq!(d.items.len(), 3, "{:?}", d.items);
    assert_eq!(d.clusters.len(), 1);
    let members = &d.clusters[0].members;
    assert_eq!(
        members,
        &vec![f.code.join("a/node_modules"), f.code.join("c/node_modules")]
    );
    assert!(!d.partial_scan);
}

#[test]
fn newest_mtime_should_be_the_2h_old_file_not_dir_age_When_dir_mtime_60d() {
    let f = fixture();
    let nm = node_checkout(&f.code.join("a"), None);
    set_age(&nm.join("x"), 2 * HOUR);
    set_age(&nm, 60 * DAY);

    let roots = roots_for(&f);
    let ops = RealDirOps::confined_to(&f.home);
    let (probe, clock) = (FakeProbe::new(), FakeClock::at(now_epoch()));
    let env = BuildEnv {
        ops: &ops,
        roots: &roots,
        probe: &probe,
        clock: &clock,
    };
    let d = discover_build_dirs(&env, &opts()).unwrap();
    let item = &d.items[0];
    assert!(matches!(item.verdict, BuildVerdict::Will(_)), "{item:?}");
    let newest = item.candidate.newest_mtime().unwrap();
    let want = now_epoch() - 2 * 3600;
    assert!((newest - want).abs() <= 5, "newest {newest} want {want}");
    let dir_age_mtime = now_epoch() - 60 * 86_400;
    assert!(
        (newest - dir_age_mtime).abs() > 86_400,
        "used the dir's own age"
    );
}

#[test]
fn newest_mtime_probe_should_stop_after_first_newer_entry_When_floor_given() {
    let f = fixture();
    let nm = f.code.join("nm");
    fs::create_dir_all(&nm).unwrap();
    for i in 0..50 {
        fs::write(nm.join(format!("f{i}")), "x").unwrap();
    }
    let st = fs::symlink_metadata(&nm).unwrap();
    let ops = FaultDirOps::wrap(RealDirOps::confined_to(&f.home)).build();
    let target = ProbeTarget {
        path: &nm,
        ident: Identity::of(&st),
        own: (0, 0),
        root_dev: RootDev(std::os::unix::fs::MetadataExt::dev(&st)),
    };
    let floor = std::time::SystemTime::now() - HOUR;
    let p = probe_newest_mtime(&ops, target, floor, &mut NewestStore::in_memory()).unwrap();
    assert!(matches!(p.recency, Recency::Young { .. }));
    assert_eq!(ops.calls(Op::StatAt), 1, "stops at the first newer entry");
}

#[test]
fn newest_mtime_probe_should_not_run_When_cheaper_guard_already_skipped_candidate() {
    let f = fixture();
    let roots = roots_for(&f);
    // OutsideRoot: a real node_modules beside the scan root
    let outside = node_checkout(&f.home.join("elsewhere/p"), None);
    // Symlink: a node_modules that is a link to a real directory
    let real = f.home.join("real");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("x"), "x").unwrap();
    fs::create_dir_all(f.code.join("s")).unwrap();
    fs::write(f.code.join("s/package.json"), "{}").unwrap();
    symlink(&real, f.code.join("s/node_modules")).unwrap();
    // KindNotSelected: a cargo target while only node_modules is selected
    fs::create_dir_all(f.code.join("k/target")).unwrap();
    fs::write(f.code.join("k/Cargo.toml"), "").unwrap();
    fs::write(f.code.join("k/target/y"), "y").unwrap();
    // TooRecent by the directory's own mtime
    let young = node_checkout(&f.code.join("y"), None);

    let ops = FaultDirOps::wrap(RealDirOps::confined_to(&f.home)).build();
    let (probe, clock) = (FakeProbe::new(), FakeClock::at(now_epoch()));
    let env = BuildEnv {
        ops: &ops,
        roots: &roots,
        probe: &probe,
        clock: &clock,
    };
    let mut o = opts();
    o.kinds = BuildKinds {
        node_modules: true,
        cargo_target: false,
    };
    let raws = vec![
        raw(&outside, BuildDirFlavor::NodeModules, None),
        raw(
            &f.code.join("s/node_modules"),
            BuildDirFlavor::NodeModules,
            None,
        ),
        raw(&f.code.join("k/target"), BuildDirFlavor::CargoTarget, None),
        raw(&young, BuildDirFlavor::NodeModules, None),
    ];
    let d = decide_build_dirs(&env, &raws, false, &o, &mut NewestStore::in_memory());
    let reasons: Vec<_> = d
        .items
        .iter()
        .map(|i| match &i.verdict {
            BuildVerdict::Skip(r) => *r,
            BuildVerdict::Will(_) => panic!("unexpected Will: {i:?}"),
        })
        .collect();
    assert_eq!(
        reasons,
        vec![
            SkipReason::OutsideRoot,
            SkipReason::Symlink,
            SkipReason::KindNotSelected,
            SkipReason::TooRecent
        ]
    );
    assert_eq!(ops.calls(Op::StatAt), 0);
    assert_eq!(ops.calls(Op::Fstat), 0);
    assert_eq!(ops.calls(Op::OpenRoot), 0);
}

#[test]
fn partial_scan_should_set_partial_scan_keep_clustered_build_dirs_and_rescan_should_disable_the_walker_cache_When_max_seconds_1(
) {
    let f = fixture();
    let a = node_checkout(&f.code.join("a"), Some("same"));
    let c = node_checkout(&f.code.join("c"), Some("same"));
    let lone = node_checkout(&f.code.join("l"), None);
    for d in [&a, &c, &lone] {
        set_age(&d.join("x"), 5 * DAY);
        set_age(d, 5 * DAY);
    }
    let roots = roots_for(&f);
    let ops = RealDirOps::confined_to(&f.home);
    let (probe, clock) = (FakeProbe::new(), FakeClock::at(now_epoch()));
    let env = BuildEnv {
        ops: &ops,
        roots: &roots,
        probe: &probe,
        clock: &clock,
    };
    let raws = vec![
        raw(&a, BuildDirFlavor::NodeModules, Some(7)),
        raw(&c, BuildDirFlavor::NodeModules, Some(7)),
        raw(&lone, BuildDirFlavor::NodeModules, None),
    ];
    let d = decide_build_dirs(&env, &raws, true, &opts(), &mut NewestStore::in_memory());
    assert!(d.partial_scan);
    for i in &d.items[..2] {
        assert!(
            matches!(i.verdict, BuildVerdict::Skip(SkipReason::KeptAsNewest)),
            "{i:?}"
        );
        assert_eq!(
            i.note.as_deref(),
            Some("cluster may be incomplete (partial scan)")
        );
    }
    assert!(
        matches!(d.items[2].verdict, BuildVerdict::Will(_)),
        "no lockfile hash: judged by age alone"
    );

    // --max-seconds maps to max_duration and a cut-short walk is reported as partial
    let mut o = opts();
    o.max_duration = Some(Duration::ZERO);
    assert_eq!(scan_options(&roots, &o).max_duration, Some(Duration::ZERO));
    let cut = discover_build_dirs(&env, &o).unwrap();
    assert!(cut.partial_scan, "a walk past --max-seconds is truncated");

    // --rescan: the walker cache is neither read nor trusted
    let mut o = opts();
    o.walker_cache = Some(f.home.join("walker.cache"));
    assert!(scan_options(&roots, &o).cache.is_some());
    o.rescan = true;
    assert!(scan_options(&roots, &o).cache.is_none());
}

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names,
    non_snake_case
)]
//! The lazy probe and its sidecar against generated fixtures (Story 1.3.1, spike 0.2.6; validation T-517,
//! T-518).

mod common;
use common::discovery::*;

use std::fs;
use std::os::unix::fs::symlink;
use std::time::{Duration, SystemTime};

use dunnage_adapters::discover_build::{
    decide_build_dirs, BuildEnv, BuildKinds, BuildOpts, BuildVerdict, RawBuildDir,
};
use dunnage_adapters::mtime_probe::NewestStore;
use dunnage_adapters::testing::{FaultDirOps, Op};
use dunnage_adapters::RealDirOps;
use dunnage_core::testing::{FakeClock, FakeProbe};
use dunnage_core::{BuildDirFlavor, Identity, SkipReason};

fn opts() -> BuildOpts {
    BuildOpts {
        min_age: HOUR,
        kinds: BuildKinds {
            node_modules: true,
            cargo_target: false,
        },
        max_duration: None,
        rescan: false,
        walker_cache: None,
    }
}

fn raw(path: &std::path::Path, flavor: BuildDirFlavor) -> RawBuildDir {
    RawBuildDir {
        path: path.to_path_buf(),
        flavor,
        group: path.parent().unwrap().to_path_buf(),
        lockfile_hash: None,
        est_bytes: 4096,
    }
}

/// `dir` holding `n` files, every file and then the directory set `age` old.
fn populated(dir: &std::path::Path, n: usize, age: Duration) {
    fs::create_dir_all(dir).unwrap();
    for i in 0..n {
        let p = dir.join(format!("f{i}"));
        fs::write(&p, "x").unwrap();
        set_age(&p, age);
    }
    set_age(dir, age);
}

#[test]
fn probe_should_make_StatAt_calls_only_under_the_10_Will_candidates_When_190_of_200_build_dirs_are_skipped_first(
) {
    let f = fixture();
    let roots = roots_for(&f);
    let mut raws = Vec::new();
    let mut will = 0;
    for i in 0..200 {
        let dir = f.code.join(format!("p{i}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("package.json"), "{}").unwrap();
        let nm = dir.join("node_modules");
        match i % 20 {
            // 10 Will candidates: old files, old directory
            0 => {
                populated(&nm, 50, 3 * DAY);
                will += 1;
                raws.push(raw(&nm, BuildDirFlavor::NodeModules));
            }
            // OutsideRoot: lives beside, not below, the scan root
            1..=5 => {
                let out = f.home.join(format!("out{i}/node_modules"));
                populated(&out, 50, 3 * DAY);
                raws.push(raw(&out, BuildDirFlavor::NodeModules));
            }
            // Symlink
            6..=10 => {
                let real = f.home.join(format!("real{i}"));
                populated(&real, 50, 3 * DAY);
                symlink(&real, &nm).unwrap();
                raws.push(raw(&nm, BuildDirFlavor::NodeModules));
            }
            // kind not selected
            11..=15 => {
                let tg = dir.join("target");
                populated(&tg, 50, 3 * DAY);
                raws.push(raw(&tg, BuildDirFlavor::CargoTarget));
            }
            // TooRecent by the directory's own mtime
            _ => {
                populated(&nm, 50, 3 * DAY);
                set_age(&nm, Duration::from_secs(60));
                raws.push(raw(&nm, BuildDirFlavor::NodeModules));
            }
        }
    }
    assert_eq!(raws.len(), 200);
    assert_eq!(will, 10);

    let ops = FaultDirOps::wrap(RealDirOps::confined_to(&f.home)).build();
    let (probe, clock) = (FakeProbe::new(), FakeClock::at(now_epoch()));
    let env = BuildEnv {
        ops: &ops,
        roots: &roots,
        probe: &probe,
        clock: &clock,
    };
    let d = decide_build_dirs(&env, &raws, false, &opts(), &mut NewestStore::in_memory());
    let wills = d
        .items
        .iter()
        .filter(|i| matches!(i.verdict, BuildVerdict::Will(_)))
        .count();
    assert_eq!(wills, 10);
    assert_eq!(
        ops.calls(Op::StatAt),
        500,
        "50 per Will directory, none under the 190 skipped (an eager probe makes 10000)"
    );
    assert_eq!(d.probes.walked, 10);
}

fn run_once(
    f: &Fixture,
    d: &std::path::Path,
    store: &mut NewestStore,
) -> (BuildVerdict, usize, usize, usize) {
    let roots = roots_for(f);
    let ops = FaultDirOps::wrap(RealDirOps::confined_to(&f.home)).build();
    let (probe, clock) = (FakeProbe::new(), FakeClock::at(now_epoch()));
    let env = BuildEnv {
        ops: &ops,
        roots: &roots,
        probe: &probe,
        clock: &clock,
    };
    let out = decide_build_dirs(
        &env,
        &[raw(d, BuildDirFlavor::NodeModules)],
        false,
        &opts(),
        store,
    );
    let item = out.items.into_iter().next().unwrap();
    (
        item.verdict,
        ops.calls(Op::StatAt),
        out.probes.walked,
        out.probes.from_sidecar,
    )
}

#[test]
fn stored_newest_mtime_should_end_a_probe_for_a_young_unchanged_dir_and_never_prove_an_old_one_When_sidecar_beside_walker_cache(
) {
    let f = fixture();
    let walker_cache = f.home.join("walker.cache");
    let sidecar = NewestStore::sidecar_path(&walker_cache);
    assert_eq!(sidecar, f.home.join("walker.cache.newest"));

    // D: directory mtime 3 days old, newest file inside 10 minutes old (below the 60 minute floor)
    let d = f.code.join("d/node_modules");
    populated(&d, 50, 3 * DAY);
    set_age(&d.join("f7"), Duration::from_secs(600));
    set_age(&d, 3 * DAY);

    // control, no sidecar: a full probe every run
    let (v, statat, walked, _) = run_once(&f, &d, &mut NewestStore::in_memory());
    assert!(matches!(v, BuildVerdict::Skip(SkipReason::TooRecent)));
    assert!(
        statat > 0 && walked == 1,
        "without a sidecar the tree is walked"
    );

    // first run with a sidecar walks and stores
    let mut store = NewestStore::open(sidecar.clone());
    let (v, statat, ..) = run_once(&f, &d, &mut store);
    assert!(matches!(v, BuildVerdict::Skip(SkipReason::TooRecent)));
    assert!(statat > 0);
    store.save().unwrap();
    assert!(sidecar.exists());

    // second run: unchanged identity and own mtime, young stored value, zero syscalls under D
    let mut store = NewestStore::open(sidecar.clone());
    assert_eq!(store.len(), 1);
    let (v, statat, walked, from_sidecar) = run_once(&f, &d, &mut store);
    assert!(
        matches!(v, BuildVerdict::Skip(SkipReason::TooRecent)),
        "{v:?}"
    );
    assert_eq!(statat, 0);
    assert_eq!((walked, from_sidecar), (0, 1));

    // --rescan: the sidecar is not trusted, the tree is walked again
    let mut store = NewestStore::open_ignoring_contents(sidecar.clone());
    let (_, statat, ..) = run_once(&f, &d, &mut store);
    assert!(statat > 0);

    // a stored value 2 hours old never proves a directory old: a file deep inside was written 5 minutes
    // ago without moving the directory's own mtime
    let e = f.code.join("e/node_modules");
    populated(&e, 5, 3 * DAY);
    fs::create_dir_all(e.join("sub/deep")).unwrap();
    fs::write(e.join("sub/deep/z"), "z").unwrap();
    set_age(&e.join("sub/deep/z"), Duration::from_secs(300));
    set_age(&e.join("sub/deep"), 3 * DAY);
    set_age(&e.join("sub"), 3 * DAY);
    set_age(&e, 3 * DAY);
    let m = fs::symlink_metadata(&e).unwrap();
    let own = (
        std::os::unix::fs::MetadataExt::mtime(&m),
        u32::try_from(std::os::unix::fs::MetadataExt::mtime_nsec(&m)).unwrap(),
    );
    let mut store = NewestStore::in_memory();
    store.record(Identity::of(&m), own, SystemTime::now() - 2 * HOUR);
    let (v, statat, walked, _) = run_once(&f, &e, &mut store);
    assert!(
        matches!(v, BuildVerdict::Skip(SkipReason::TooRecent)),
        "the stored 2 h value must not make a directory with a 5 minute old file Will: {v:?}"
    );
    assert!(statat > 0 && walked == 1);
}

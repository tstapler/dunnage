#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names,
    non_snake_case
)]
//! Delete-time re-check of build dirs against real fixtures (Story 3.2.1).

mod common;
use common::discovery::*;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::Path;
use std::time::Duration;

use dunnage_adapters::mtime_probe::NewestStore;
use dunnage_adapters::targets::build_dir::{recheck_build_dir, BuildRecheck, RecheckEnv};
use dunnage_adapters::testing::{At, FaultDirOps};
use dunnage_adapters::{delete_tree, open_parent, open_verified_root, RealDirOps};
use dunnage_core::targets::build_dir::BuildGuard;
use dunnage_core::testing::FakeProbe;
use dunnage_core::{
    AbortFlag, AllowedTarget, ApplyToken, BuildDirFlavor, DeleteError, DeletePolicy, Identity,
    NamePattern, RootDev, SkipReason,
};

/// Ages a symlink itself (`File::open` would follow it).
fn age_link(p: &Path, age: Duration) {
    let t = std::time::SystemTime::now() - age;
    let d = t.duration_since(std::time::UNIX_EPOCH).unwrap();
    let ts = rustix::fs::Timespec {
        tv_sec: i64::try_from(d.as_secs()).unwrap(),
        tv_nsec: 0,
    };
    rustix::fs::utimensat(
        rustix::fs::CWD,
        p,
        &rustix::fs::Timestamps {
            last_access: ts,
            last_modification: ts,
        },
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
}

fn ident(p: &Path) -> Identity {
    Identity::of(&fs::symlink_metadata(p).unwrap())
}

fn run<D: dunnage_adapters::DirOps>(
    f: &Fixture,
    ops: &D,
    flavor: BuildDirFlavor,
    path: &Path,
    planned: Identity,
    min_age: Duration,
) -> BuildRecheck {
    let roots = roots_for(f);
    let probe = FakeProbe::new();
    let env = RecheckEnv {
        ops,
        roots: &roots,
        probe: &probe,
    };
    recheck_build_dir(
        &env,
        flavor,
        path,
        planned,
        now_epoch(),
        min_age,
        &mut NewestStore::in_memory(),
    )
}

fn reason(r: &BuildRecheck) -> Option<(SkipReason, Option<String>)> {
    match &r.guard {
        BuildGuard::Skip { reason, detail } => Some((*reason, detail.clone())),
        BuildGuard::Allow(_) => None,
    }
}

/// An aged `app/node_modules/x` with a lockfile; returns its path.
fn old_node(f: &Fixture, age: Duration) -> std::path::PathBuf {
    let nm = node_checkout(&f.code.join("app"), Some("lock"));
    set_age(&nm.join("x"), age);
    set_age(&nm, age);
    nm
}

#[test]
fn recheck_should_allow_an_old_build_dir_with_a_lockfile() {
    let f = fixture();
    let nm = old_node(&f, 2 * HOUR);
    let ops = RealDirOps::confined_to(&f.home);
    let r = run(&f, &ops, BuildDirFlavor::NodeModules, &nm, ident(&nm), HOUR);
    assert!(reason(&r).is_none(), "{r:?}");
    assert!(r.newest_mtime.is_some());
}

#[test]
fn recheck_should_skip_identity_changed_When_package_json_was_deleted_after_the_scan() {
    let f = fixture();
    let nm = old_node(&f, 2 * HOUR);
    let planned = ident(&nm);
    fs::remove_file(f.code.join("app/package.json")).unwrap();
    let ops = RealDirOps::confined_to(&f.home);
    let r = run(&f, &ops, BuildDirFlavor::NodeModules, &nm, planned, HOUR);
    assert_eq!(reason(&r).unwrap().0, SkipReason::IdentityChanged);
}

#[test]
fn recheck_should_skip_symlink_and_leave_the_target_alone_When_target_became_a_link() {
    let f = fixture();
    let app = f.code.join("app");
    fs::create_dir_all(app.join("target")).unwrap();
    fs::write(app.join("Cargo.toml"), "[package]").unwrap();
    let planned = ident(&app.join("target"));
    fs::remove_dir_all(app.join("target")).unwrap();
    let elsewhere = f.home.parent().unwrap().to_path_buf().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("keep"), "k").unwrap();
    symlink(&elsewhere, app.join("target")).unwrap();
    let ops = RealDirOps::confined_to(&f.home);

    let r = run(
        &f,
        &ops,
        BuildDirFlavor::CargoTarget,
        &app.join("target"),
        planned,
        HOUR,
    );

    assert_eq!(reason(&r).unwrap().0, SkipReason::Symlink);
    assert_eq!(fs::read_to_string(elsewhere.join("keep")).unwrap(), "k");
}

#[test]
fn recheck_should_skip_cross_device_and_delete_tree_should_refuse_it_When_fstat_reports_other_dev()
{
    let f = fixture();
    let nm = old_node(&f, 2 * HOUR);
    let planned = ident(&nm);
    let mk = || {
        FaultDirOps::wrap(RealDirOps::confined_to(&f.home))
            .report_dev(At::Name(OsString::from("node_modules")), 34)
            .build()
    };
    let ops = mk();

    let r = run(&f, &ops, BuildDirFlavor::NodeModules, &nm, planned, HOUR);
    assert_eq!(reason(&r).unwrap().0, SkipReason::CrossDevice);

    // the same directory handed to delete_tree: nothing is removed
    let root = open_verified_root(&ops, &f.home, ident(&f.home)).unwrap();
    let parent = open_parent(&ops, &root, Path::new("code/app")).unwrap();
    let pol = DeletePolicy {
        root_dev: RootDev(fs::metadata(&f.code).unwrap().dev()),
        root_mnt: None,
        home: f.home.clone(),
        home_id: ident(&f.home),
        scan_roots: vec![f.code.clone()],
        allowed: AllowedTarget::Within {
            root: f.code.clone(),
            name: NamePattern::NodeModules,
        },
        target: nm.clone(),
        require_sibling_any: vec![OsString::from("package.json")],
        max_depth: 256,
        restore_owner_write: false,
    };
    let out = delete_tree(
        &ops,
        &parent,
        OsStr::new("node_modules"),
        // a real nested subvolume carries its own st_dev in the identity taken at plan time
        Identity {
            dev: 34,
            ino: planned.ino,
        },
        &pol,
        &AbortFlag::default(),
        &ApplyToken::for_tests(),
    );
    assert_eq!(out.unwrap_err().error, DeleteError::CrossDevice);
    assert!(nm.join("x").exists());
}

#[test]
fn recheck_should_skip_too_recent_When_newest_is_20_minutes_old_for_any_min_age_flag() {
    let f = fixture();
    let nm = old_node(&f, 2 * HOUR);
    set_age(&nm.join("x"), Duration::from_mins(20));
    set_age(&nm, 2 * HOUR);
    let planned = ident(&nm);
    let ops = RealDirOps::confined_to(&f.home);
    for flag in [Duration::ZERO, Duration::from_secs(300), HOUR] {
        let r = run(&f, &ops, BuildDirFlavor::NodeModules, &nm, planned, flag);
        let (why, detail) = reason(&r).expect("skipped");
        assert_eq!(why, SkipReason::TooRecent, "{flag:?}");
        let detail = detail.unwrap();
        assert!(detail.starts_with("younger than --min-age 60m"), "{detail}");
        assert_eq!(detail.ends_with("(build-dir floor)"), flag < HOUR);
    }
}

#[test]
fn recheck_should_allow_When_newest_is_61_minutes_old_and_flag_is_5m() {
    let f = fixture();
    let nm = old_node(&f, Duration::from_mins(61));
    let ops = RealDirOps::confined_to(&f.home);
    let r = run(
        &f,
        &ops,
        BuildDirFlavor::NodeModules,
        &nm,
        ident(&nm),
        Duration::from_secs(300),
    );
    assert!(reason(&r).is_none(), "{r:?}");
}

#[test]
fn recheck_should_skip_not_reproducible_When_an_npm_linked_package_points_outside() {
    let f = fixture();
    let nm = old_node(&f, 2 * HOUR);
    let outside = f.home.join("libs/mylib");
    fs::create_dir_all(&outside).unwrap();
    symlink(&outside, nm.join("mylib")).unwrap();
    age_link(&nm.join("mylib"), 2 * HOUR);
    set_age(&nm, 2 * HOUR);
    let ops = RealDirOps::confined_to(&f.home);
    let r = run(&f, &ops, BuildDirFlavor::NodeModules, &nm, ident(&nm), HOUR);
    assert_eq!(
        reason(&r),
        Some((
            SkipReason::NotReproducible,
            Some("an npm-linked package points outside the project".into())
        ))
    );
}

#[test]
fn recheck_should_skip_not_reproducible_When_a_bin_link_or_scoped_link_leaves_the_project() {
    let f = fixture();
    let nm = old_node(&f, 2 * HOUR);
    fs::create_dir_all(nm.join(".bin")).unwrap();
    symlink("../../../../../etc/passwd", nm.join(".bin/tool")).unwrap();
    age_link(&nm.join(".bin/tool"), 2 * HOUR);
    for p in [nm.join(".bin"), nm.clone()] {
        set_age(&p, 2 * HOUR);
    }
    let ops = RealDirOps::confined_to(&f.home);
    let r = run(&f, &ops, BuildDirFlavor::NodeModules, &nm, ident(&nm), HOUR);
    assert_eq!(reason(&r).unwrap().0, SkipReason::NotReproducible);
    fs::remove_dir_all(nm.join(".bin")).unwrap();
    fs::create_dir_all(nm.join("@sc")).unwrap();
    symlink("/tmp", nm.join("@sc/pkg")).unwrap();
    age_link(&nm.join("@sc/pkg"), 2 * HOUR);
    for p in [nm.join("@sc"), nm.join("x"), nm.clone()] {
        set_age(&p, 2 * HOUR);
    }
    let r = run(&f, &ops, BuildDirFlavor::NodeModules, &nm, ident(&nm), HOUR);
    assert_eq!(reason(&r).unwrap().0, SkipReason::NotReproducible);
}

#[test]
fn recheck_should_skip_not_reproducible_When_no_lockfile_sits_next_to_node_modules() {
    let f = fixture();
    let nm = node_checkout(&f.code.join("app"), None);
    set_age(&nm.join("x"), 2 * HOUR);
    set_age(&nm, 2 * HOUR);
    let ops = RealDirOps::confined_to(&f.home);
    let r = run(&f, &ops, BuildDirFlavor::NodeModules, &nm, ident(&nm), HOUR);
    assert_eq!(
        reason(&r),
        Some((
            SkipReason::NotReproducible,
            Some("no lockfile next to node_modules".into())
        ))
    );
}

#[test]
fn recheck_should_allow_a_pnpm_tree_whose_links_stay_inside_and_a_cargo_target_without_lockfile() {
    let f = fixture();
    let app = f.code.join("app");
    let nm = node_checkout(&app, None);
    fs::write(app.join("pnpm-lock.yaml"), "l").unwrap();
    fs::create_dir_all(nm.join(".pnpm/real@1/node_modules/real")).unwrap();
    symlink(".pnpm/real@1/node_modules/real", nm.join("real")).unwrap();
    age_link(&nm.join("real"), 2 * HOUR);
    for p in [
        nm.join(".pnpm/real@1/node_modules/real"),
        nm.join(".pnpm/real@1/node_modules"),
        nm.join(".pnpm/real@1"),
    ] {
        set_age(&p, 2 * HOUR);
    }
    for p in [&nm.join("x"), &nm.join(".pnpm"), &nm] {
        set_age(p, 2 * HOUR);
    }
    let ops = RealDirOps::confined_to(&f.home);
    let r = run(&f, &ops, BuildDirFlavor::NodeModules, &nm, ident(&nm), HOUR);
    assert!(reason(&r).is_none(), "{r:?}");

    let b = f.code.join("b");
    fs::create_dir_all(b.join("target")).unwrap();
    fs::write(b.join("Cargo.toml"), "[package]").unwrap();
    fs::write(b.join("target/o"), "o").unwrap();
    set_age(&b.join("target/o"), 2 * HOUR);
    set_age(&b.join("target"), 2 * HOUR);
    let r = run(
        &f,
        &ops,
        BuildDirFlavor::CargoTarget,
        &b.join("target"),
        ident(&b.join("target")),
        HOUR,
    );
    assert!(reason(&r).is_none(), "{r:?}");
}

#[test]
fn recheck_should_skip_gone_When_the_directory_was_removed() {
    let f = fixture();
    let nm = old_node(&f, 2 * HOUR);
    let planned = ident(&nm);
    fs::remove_dir_all(&nm).unwrap();
    let ops = RealDirOps::confined_to(&f.home);
    let r = run(&f, &ops, BuildDirFlavor::NodeModules, &nm, planned, HOUR);
    assert_eq!(reason(&r).unwrap().0, SkipReason::Gone);
}

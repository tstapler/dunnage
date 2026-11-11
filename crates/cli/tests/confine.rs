#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 2.1.6: binary-level confinement. The table test drives the pure `confine_mode`; the spawn tests run
//! the real binary under `cargo test` (and `cargo test --release`, which `make safety` also runs).

mod common;

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use common::Fixture;
use disk_cleanup_adapters::command_confined::ConfinedCommandRunner;
use disk_cleanup_adapters::dirops::is_scope_violation;
use disk_cleanup_adapters::{DirOps, RealCommandRunner};
use disk_cleanup_cli::wiring::confine::{
    apply_dir_ops, apply_runner, confine_mode, Confine, Vars, BAD_ROOT, DEBUG_REFUSES,
    TEST_ENV_REFUSES,
};
use disk_cleanup_core::ports::{Cmd, CommandRunner};
use disk_cleanup_core::{ApplyToken, DeleteError};

const RELEASE: &str = "dc-flavor=release";
const TEST_SUPPORT: &str = "dc-flavor=test-support;9f3ac7e1";

fn vars(pairs: &[(&str, &OsStr)]) -> Vars {
    pairs
        .iter()
        .map(|(k, v)| (OsString::from(k), (*v).to_owned()))
        .collect()
}

fn canon(p: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(p).unwrap()
}

#[test]
fn confine_mode_should_follow_its_input_table() {
    let fx = Fixture::new();
    let root = fx.root().as_os_str();
    let root_c = canon(fx.root());
    let no_sentinel = tempfile::tempdir().unwrap();
    let a_file = fx.root().join("a-file");
    std::fs::write(&a_file, "").unwrap();
    let link_bare = fx.root().join("link-to-bare");
    std::os::unix::fs::symlink(no_sentinel.path(), &link_bare).unwrap();
    let link_fx = no_sentinel.path().join("link-to-fixture");
    std::os::unix::fs::symlink(fx.root(), &link_fx).unwrap();
    let parent = fx.root().parent().unwrap().as_os_str().to_owned();

    // (name, vars, flavor, debug_assertions, expected)
    let one = OsString::from("1");
    let empty = OsString::new();
    let rows: Vec<(&str, Vars, &str, bool, Confine)> = vec![
        (
            "fixture with sentinel, release flavor",
            vars(&[("DC_TEST_ROOT", root)]),
            RELEASE,
            false,
            Confine::Root(root_c.clone()),
        ),
        (
            "fixture with sentinel, test-support flavor, debug",
            vars(&[("DC_TEST_ROOT", root)]),
            TEST_SUPPORT,
            true,
            Confine::Root(root_c.clone()),
        ),
        (
            "fixture, cargo env present",
            vars(&[("DC_TEST_ROOT", root), ("CARGO_MANIFEST_DIR", &empty)]),
            TEST_SUPPORT,
            true,
            Confine::Root(root_c.clone()),
        ),
        (
            "confinement beats DC_DEV_APPLY_REAL",
            vars(&[("DC_TEST_ROOT", root), ("DC_DEV_APPLY_REAL", &one)]),
            RELEASE,
            false,
            Confine::Root(root_c.clone()),
        ),
        (
            "symlink to the fixture is canonicalised",
            vars(&[("DC_TEST_ROOT", link_fx.as_os_str())]),
            RELEASE,
            false,
            Confine::Root(root_c.clone()),
        ),
        (
            "relative",
            vars(&[("DC_TEST_ROOT", OsStr::new("relative/dir"))]),
            RELEASE,
            false,
            Confine::Refuse(BAD_ROOT),
        ),
        (
            "empty",
            vars(&[("DC_TEST_ROOT", &empty)]),
            RELEASE,
            false,
            Confine::Refuse(BAD_ROOT),
        ),
        (
            "missing",
            vars(&[("DC_TEST_ROOT", OsStr::new("/nonexistent-dc-root"))]),
            RELEASE,
            false,
            Confine::Refuse(BAD_ROOT),
        ),
        (
            "a file",
            vars(&[("DC_TEST_ROOT", a_file.as_os_str())]),
            RELEASE,
            false,
            Confine::Refuse(BAD_ROOT),
        ),
        (
            "slash",
            vars(&[("DC_TEST_ROOT", OsStr::new("/"))]),
            RELEASE,
            false,
            Confine::Refuse(BAD_ROOT),
        ),
        (
            "directory without the sentinel",
            vars(&[("DC_TEST_ROOT", no_sentinel.path().as_os_str())]),
            RELEASE,
            false,
            Confine::Refuse(BAD_ROOT),
        ),
        (
            "parent of the fixture (leaked ancestor)",
            vars(&[("DC_TEST_ROOT", &parent)]),
            RELEASE,
            false,
            Confine::Refuse(BAD_ROOT),
        ),
        (
            "symlink to a directory without the sentinel",
            vars(&[("DC_TEST_ROOT", link_bare.as_os_str())]),
            RELEASE,
            false,
            Confine::Refuse(BAD_ROOT),
        ),
        (
            "debug build, no root",
            vars(&[]),
            RELEASE,
            true,
            Confine::Refuse(DEBUG_REFUSES),
        ),
        (
            "debug build, DC_DEV_APPLY_REAL=1",
            vars(&[("DC_DEV_APPLY_REAL", &one)]),
            RELEASE,
            true,
            Confine::Unconfined,
        ),
        (
            "debug build, DC_DEV_APPLY_REAL=0 is not the opt-in",
            vars(&[("DC_DEV_APPLY_REAL", OsStr::new("0"))]),
            RELEASE,
            true,
            Confine::Refuse(DEBUG_REFUSES),
        ),
        (
            "release build, plain",
            vars(&[]),
            RELEASE,
            false,
            Confine::Unconfined,
        ),
        (
            "test-support flavor, no root",
            vars(&[]),
            TEST_SUPPORT,
            false,
            Confine::Refuse(TEST_ENV_REFUSES),
        ),
        (
            "test-support flavor beats DC_DEV_APPLY_REAL",
            vars(&[("DC_DEV_APPLY_REAL", &one)]),
            TEST_SUPPORT,
            true,
            Confine::Refuse(TEST_ENV_REFUSES),
        ),
        (
            "CARGO_MANIFEST_DIR, release flavor",
            vars(&[("CARGO_MANIFEST_DIR", &empty)]),
            RELEASE,
            false,
            Confine::Refuse(TEST_ENV_REFUSES),
        ),
        (
            "CARGO_BIN_EXE_*, release flavor, DC_DEV_APPLY_REAL",
            vars(&[
                ("CARGO_BIN_EXE_disk-cleanup", &empty),
                ("DC_DEV_APPLY_REAL", &one),
            ]),
            RELEASE,
            true,
            Confine::Refuse(TEST_ENV_REFUSES),
        ),
        (
            "CARGO_PRIMARY_PACKAGE alone is not a signal",
            vars(&[("CARGO_PRIMARY_PACKAGE", &one)]),
            RELEASE,
            false,
            Confine::Unconfined,
        ),
    ];
    for (name, v, flavor, debug, want) in rows {
        assert_eq!(confine_mode(&v, flavor, debug), want, "row: {name}");
    }
}

#[test]
fn confined_dir_ops_should_refuse_a_mutation_outside_the_root_and_allow_one_inside() {
    let fx = Fixture::new();
    let mode = confine_mode(
        &vars(&[("DC_TEST_ROOT", fx.root().as_os_str())]),
        RELEASE,
        false,
    );
    let token = ApplyToken::for_tests();
    let ops = apply_dir_ops(&mode, &token).expect("confined ops");
    let inside = canon(&fx.home());
    std::fs::write(inside.join("victim"), "x").unwrap();
    let dir = ops.open_root(&inside).unwrap();
    ops.unlinkat(&dir, OsStr::new("victim"), false).unwrap();
    assert!(!inside.join("victim").exists());

    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("keep"), "x").unwrap();
    let odir = ops.open_root(&canon(outside.path())).unwrap();
    let err = ops.unlinkat(&odir, OsStr::new("keep"), false).unwrap_err();
    assert!(is_scope_violation(&err), "{err}");
    assert!(outside.path().join("keep").exists());

    assert!(apply_dir_ops(&Confine::Refuse(BAD_ROOT), &token).is_none());
    assert!(apply_runner(&Confine::Refuse(BAD_ROOT)).is_none());
}

fn shim(dir: &Path, name: &str, marker: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    std::fs::write(
        &p,
        format!("#!/bin/sh\necho ran > '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn confined_runner_should_run_a_shim_under_the_fixture_bin_and_refuse_a_tool_outside_it() {
    let fx = Fixture::new();
    let root = canon(fx.root());
    let token = ApplyToken::for_tests();
    let brew_cleanup = Cmd::new("brew", ["cleanup"], true);

    // a `brew` outside the fixture, first on PATH: refused, never spawned
    let decoy = tempfile::tempdir().unwrap();
    let decoy_mark = decoy.path().join("ran");
    shim(&decoy.path().join("bin"), "brew", &decoy_mark);
    let outside = ConfinedCommandRunner::with_path(
        RealCommandRunner::new(),
        root.clone(),
        decoy.path().join("bin").into_os_string(),
    );
    assert_eq!(
        outside.run_mutating(&brew_cleanup, &token),
        Err(DeleteError::Refused)
    );
    assert!(!decoy_mark.exists(), "the real tool must not run");

    // the fixture shim wins and runs
    let mark = root.join("shim-ran");
    shim(&root.join("bin"), "brew", &mark);
    let path = std::env::join_paths([root.join("bin"), decoy.path().join("bin")]).unwrap();
    let inside = ConfinedCommandRunner::with_path(RealCommandRunner::new(), root.clone(), path);
    inside
        .run_mutating(&brew_cleanup, &token)
        .expect("shim runs");
    assert!(mark.exists());

    // the classifier still applies behind the confinement
    let other = Cmd::new("brew", ["install", "x"], true);
    assert_eq!(
        inside.run_mutating(&other, &token),
        Err(DeleteError::Refused)
    );
}

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_disk-cleanup"))
}

#[test]
fn spawn_without_fixture_cmd_should_exit_64_with_and_without_the_cargo_environment() {
    for scrub in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = binary();
        c.arg("--apply")
            .env("HOME", tmp.path())
            .env("XDG_STATE_HOME", tmp.path());
        c.env_remove("DC_TEST_ROOT");
        if scrub {
            for (k, _) in std::env::vars_os() {
                if k.to_string_lossy().starts_with("CARGO_") {
                    c.env_remove(k);
                }
            }
        }
        let out = c.output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(64), "scrub={scrub}: {err}");
        assert!(
            err.contains("refusing unconfined --apply in a test or cargo environment"),
            "scrub={scrub}: {err}"
        );
        assert_eq!(
            std::fs::read_dir(tmp.path()).unwrap().count(),
            0,
            "nothing written"
        );
    }
}

#[test]
fn a_leaked_dev_apply_real_should_not_reopen_the_real_home() {
    let tmp = tempfile::tempdir().unwrap();
    let out = binary()
        .arg("--apply")
        .env_remove("DC_TEST_ROOT")
        .env("DC_DEV_APPLY_REAL", "1")
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&out.stderr).contains("refusing unconfined --apply"));
}

#[test]
fn a_bad_dc_test_root_should_exit_64_in_every_mode() {
    let fx = Fixture::new();
    let bare = tempfile::tempdir().unwrap();
    for args in [&["--apply"][..], &[][..], &["status"][..]] {
        let mut c = fx.fixture_cmd();
        c.args(args).env("DC_TEST_ROOT", bare.path());
        let out = c.output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(64), "{args:?}: {err}");
        assert!(
            err.contains("DC_TEST_ROOT must be an existing directory that contains .dc-test-root"),
            "{err}"
        );
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn a_fixture_apply_should_pass_confinement_and_reach_the_gate_a_refusal() {
    let fx = Fixture::new();
    let out = fx.fixture_cmd().arg("--apply").output().unwrap();
    if cfg!(feature = "gate-b") {
        // A Gate B build registers `caches`: the same run passes confinement and applies (nothing to remove).
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    assert_eq!(out.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&out.stderr).contains("apply is not enabled in this build"));
}

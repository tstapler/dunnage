#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Tests of the harness itself (`tests/common/mod.rs`).

mod common;

use common::{check_start_env, extract_plan_id, tree_manifest, Fixture, ENV_ALLOWLIST, SENTINEL};
use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt;

#[test]
fn harness_should_abort_test_binary_When_dc_dev_apply_real_set_in_test_process_env() {
    let set = |k: &str| (k == "DC_DEV_APPLY_REAL").then(|| OsString::from("1"));
    assert!(check_start_env(&set)
        .unwrap_err()
        .contains("DC_DEV_APPLY_REAL"));
    assert!(check_start_env(&|_| None).is_ok());

    // The real thing: this test binary re-run with the variable set dies by signal (abort), it does not
    // merely fail one test.
    let me = std::env::current_exe().unwrap();
    let out = std::process::Command::new(me)
        .args([
            "--exact",
            "harness_probe_should_pass_When_the_environment_is_clean",
            "--nocapture",
        ])
        .env("DC_DEV_APPLY_REAL", "1")
        .output()
        .unwrap();
    assert_eq!(
        out.status.signal(),
        Some(6),
        "{:?} {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn harness_probe_should_pass_When_the_environment_is_clean() {
    let _f = Fixture::new();
}

#[test]
fn fixture_cmd_should_keep_xdg_dirs_outside_home_write_the_sentinel_and_leave_the_home_hash_unchanged_after_a_plan_run_When_spawned(
) {
    let f = Fixture::new();
    assert!(f.root().join(SENTINEL).is_file());
    for which in ["state", "cache", "config"] {
        assert!(f.xdg(which).is_dir());
        assert!(
            !f.xdg(which).starts_with(f.home()),
            "{which} must not be inside HOME"
        );
    }
    // `env_clear` plus the allowlist: the child sees exactly these variables, whatever this process has.
    let out = f.command_for("env").output().unwrap();
    let mut names: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k.to_string()))
        .collect();
    names.sort();
    let mut want: Vec<String> = ENV_ALLOWLIST.iter().map(ToString::to_string).collect();
    want.sort();
    assert_eq!(names, want);
    assert!(!names.iter().any(|n| n == "DC_DEV_APPLY_REAL"));

    // A run of the binary leaves the hashed fixture home as it was. The plan-mode run that writes the walker
    // cache under xdg/ is Story 1.2.4's CLI; `--version` is what the binary offers today.
    std::fs::write(f.home().join("keep.txt"), "x").unwrap();
    let before = tree_manifest(&f.home());
    let out = f.fixture_cmd().arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(tree_manifest(&f.home()), before);
}

#[test]
fn extract_plan_id_should_read_the_json_field_or_the_next_command_and_refuse_malformed_ids() {
    assert_eq!(
        extract_plan_id(r#"{"plan_id": "91be04d7", "x": 1}"#).as_deref(),
        Some("91be04d7")
    );
    assert_eq!(
        extract_plan_id(r#"{"next":["dunnage --kinds build-dirs --apply --plan-id 91be04d7"]}"#)
            .as_deref(),
        Some("91be04d7")
    );
    assert_eq!(extract_plan_id(r#"{"plan_id":"zzzzzzzz"}"#), None);
    assert_eq!(extract_plan_id("{}"), None);
}

#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 5.3.0, Task 5.3.0b: the state directory moved from `disk-cleanup` to `dunnage`. The binary only
//! points at the `mv` (audit logs are append-only evidence), refuses an apply when both directories exist,
//! and `audit verify` reads a log written under the old name.

mod common;

use common::Fixture;
use serde_json::json;

fn old_log(f: &Fixture) -> std::path::PathBuf {
    let dir = f.xdg("state").join("disk-cleanup");
    std::fs::create_dir_all(&dir).unwrap();
    let home = f.home().display().to_string();
    let lines = [
        json!({"type":"run_start","schema":1,"run_id":"r1","ts":1,"roots":{
            "scan_roots":[format!("{home}/code")],"cache_paths":[],"worktree_roots":[],"steam_libraries":[]}}),
        json!({"type":"result","schema":1,"run_id":"r1","ts":2,"kind":"cache","status":"removed",
            "path":format!("{home}/.npm/_cacache")}),
        json!({"type":"run_end","run_id":"r1","ts":3}),
    ]
    .map(|l| l.to_string())
    .join("\n");
    let p = dir.join("audit.jsonl");
    std::fs::write(&p, lines + "\n").unwrap();
    p
}

#[test]
fn start_should_note_the_exact_mv_and_leave_the_old_directory_alone_When_only_the_old_one_exists() {
    let f = Fixture::new();
    old_log(&f);
    let before = common::tree_manifest(&f.xdg("state"));
    let out = f.fixture_cmd().arg("kinds").output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    let (old, new) = (
        f.xdg("state").join("disk-cleanup"),
        f.xdg("state").join("dunnage"),
    );
    assert!(
        err.contains(&format!("mv {} {}", old.display(), new.display())),
        "{err}"
    );
    assert_eq!(err.matches("NOTE:").count(), 1, "{err}");
    assert_eq!(common::tree_manifest(&f.xdg("state")), before);
    assert!(!new.exists());
}

#[test]
fn apply_should_exit_64_and_name_both_directories_When_both_exist() {
    let f = Fixture::new();
    old_log(&f);
    std::fs::create_dir_all(f.xdg("state").join("dunnage")).unwrap();
    let before = common::tree_manifest(&f.xdg("state"));
    let out = f.fixture_cmd().arg("--apply").output().unwrap();
    assert_eq!(out.status.code(), Some(64), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        err.contains("disk-cleanup") && err.contains("dunnage"),
        "{err}"
    );
    assert_eq!(common::tree_manifest(&f.xdg("state")), before);
}

#[test]
fn audit_verify_should_read_a_log_written_under_the_old_name() {
    let f = Fixture::new();
    let log = old_log(&f);
    let out = f
        .fixture_cmd()
        .args(["audit", "verify", "--log"])
        .arg(&log)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("1 deletions in 1 runs, 0 outside"), "{text}");
}

#[test]
fn old_env_prefix_should_still_be_read_with_one_deprecation_note() {
    let f = Fixture::new();
    let out = f
        .fixture_cmd()
        .env("DISK_CLEANUP_VIA", "test")
        .arg("status")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        err.matches("DISK_CLEANUP_VIA is deprecated").count(),
        1,
        "{err}"
    );
    assert!(err.contains("Set DUNNAGE_VIA instead"), "{err}");
}

#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! 0.3.1 review findings 4 and 11 for `audit verify`. Written before the fixes; same author as the
//! implementation, NOT an independent one.

use std::path::{Path, PathBuf};

use dunnage_adapters::audit_verify::{verify_audit, AllowedTable, VerifyInputs};
use dunnage_cli::audit_cmd;
use dunnage_core::Dirs;
use serde_json::{json, Value};

const KIND: &str = "explorer-selection";

fn explorer_log(path: &str) -> String {
    let lines: Vec<Value> = vec![
        json!({"type":"run_start","schema":1,"run_id":"r","ts":1,"kinds":KIND,
            "roots":{"scan_roots":[],"cache_paths":[],"worktree_roots":[],"steam_libraries":[]}}),
        json!({"type":"explorer_confirm","schema":1,"run_id":"r","ts":2,"plan_id":"abcd1234","count":1,
            "bytes":10,"typed":"1","tty":true,"items":[{"path":path,"dev":1,"ino":2}]}),
        json!({"type":"intent","schema":1,"run_id":"r","ts":3,"kind":KIND,"locator":{"path":path}}),
        json!({"type":"result","schema":1,"run_id":"r","ts":4,"kind":KIND,"status":"removed","path":path}),
        json!({"type":"run_end","run_id":"r","ts":5}),
    ];
    lines
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[test]
fn safety_audit_verify_should_judge_explorer_rows_against_the_canonical_home() {
    let td = tempfile::tempdir().unwrap();
    let base = td.path().canonicalize().unwrap();
    let real = base.join("real_home");
    std::fs::create_dir_all(real.join(".ssh/keys")).unwrap();
    let link = base.join("link_home");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let log = base.join("audit.jsonl");
    std::fs::write(&log, explorer_log(real.join(".ssh/keys").to_str().unwrap())).unwrap();
    // The home as the process was given it (a symlink); the engine resolved it to `real_home`.
    let dirs = Dirs {
        home: link.clone(),
        state: link.join(".local/state"),
        cache: link.join(".cache"),
        config: link.join(".config"),
    };
    let run = audit_cmd::run(&log, &dirs, None, false);
    assert_eq!(run.code, 2, "{}{}", run.stdout, run.stderr);
    assert!(run.stdout.contains("hard floor"), "{}", run.stdout);
}

fn cargo_home_violations(paths: &[&str]) -> Vec<String> {
    let mut lines = vec![
        json!({"type":"run_start","schema":1,"run_id":"r","ts":1,"roots":{
        "scan_roots":[],"cache_paths":[],"worktree_roots":[],"steam_libraries":[]}})
        .to_string(),
    ];
    lines.extend(paths.iter().map(|p| {
        json!({"type":"result","schema":1,"run_id":"r","ts":2,"kind":"cargo-home","status":"removed","path":p})
            .to_string()
    }));
    let table = AllowedTable::embedded();
    verify_audit(
        lines.join("\n").as_bytes(),
        &VerifyInputs {
            table: &table,
            home: Path::new("/home/u"),
            config_roots: None,
        },
    )
    .unwrap()
    .violations
    .into_iter()
    .map(|v| v.path)
    .collect()
}

#[test]
fn safety_audit_verify_should_not_accept_a_cargo_home_entry_in_an_unrelated_home_folder() {
    let bad = [
        "/home/u/Documents/registry/src/a/b",
        "/home/u/Documents/git/checkouts/a/b",
        "/home/u/x/y/registry/src/index/crate-1.0.0",
    ];
    assert_eq!(cargo_home_violations(&bad), bad);
}

#[test]
fn audit_verify_should_accept_a_cargo_home_entry_below_the_default_cargo_home() {
    let ok = [
        "/home/u/.cargo/registry/src/index.crates.io-6f17d22bba15001f/serde-1.0.200",
        "/home/u/.cargo/git/checkouts/repo-1a2b3c/abcdef1",
    ];
    assert!(cargo_home_violations(&ok).is_empty());
    let _ = PathBuf::new();
}

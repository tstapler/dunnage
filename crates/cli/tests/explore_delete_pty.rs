#![cfg(all(feature = "explore", feature = "gate-d"))]
#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.5.6, Task 6.5.6d (Gate F, UX-T35, UX-T36, UX-T38): the live explorer delete on the real binary, built
//! with `gate-d` (the explorer's delete exists only there), over a fixture HOME. Keys on a pty: Space on two rows,
//! `p`, `d`, the typed count, Enter. Afterwards the directories are gone, the audit log holds `run_start`, one
//! `explorer_confirm` with the typed count, an `intent` and a `result` per item and `run_end`, `dunnage
//! audit verify` exits 0 and the closing line names the count and the measured bytes. A SIGTERM during the
//! delete ends it with 143, `run_end` says `interrupted` and the terminal is restored.
//!
//! The nested tmpfs mount needs `unshare -rm`: that journey re-runs itself inside a mount namespace (a named
//! skip where it is not allowed; `DC_REQUIRE_UNSHARE=1` makes the skip a failure). The mount is private to the
//! namespace and vanishes with it. Everything lives in a tempdir fixture HOME; nothing else is deleted.

mod common;
mod explore_pty_rig;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::{bin_path, unshare_works, Fixture};
use explore_pty_rig::Pty;
use serde_json::Value;

const SCAN_DONE: &str = "SCAN D";
const CHILD_VAR: &str = "DC_DELETE_PTY_CHILD";

fn write_config(f: &Fixture, root: &Path) {
    let dir = f.xdg("config").join("dunnage");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("config.toml"),
        format!("roots = [\"{}\"]\n", root.display()),
    )
    .unwrap();
}

fn dir_with(path: &Path, files: usize, bytes: usize) {
    fs::create_dir_all(path).unwrap();
    for i in 0..files {
        fs::write(path.join(format!("f{i}")), vec![b'x'; bytes]).unwrap();
    }
}

/// `<fixture>/home/code` with a `node_modules` (the first row) and a plain directory (the second).
fn tree(f: &Fixture) -> PathBuf {
    let code = f.home().join("code");
    dir_with(&code.join("node_modules/pkg"), 6, 6000);
    dir_with(&code.join("plain"), 3, 5000);
    fs::canonicalize(code).unwrap()
}

fn audit_lines(f: &Fixture) -> Vec<Value> {
    let text = fs::read_to_string(f.xdg("state").join("dunnage/audit.jsonl")).unwrap();
    text.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn types_of(lines: &[Value]) -> Vec<String> {
    lines
        .iter()
        .map(|l| l["type"].as_str().unwrap_or("?").to_string())
        .collect()
}

fn results(lines: &[Value]) -> Vec<(String, String)> {
    lines
        .iter()
        .filter(|l| l["type"] == "result")
        .map(|l| {
            (
                l["path"].as_str().unwrap_or("?").to_string(),
                l["status"].as_str().map_or_else(
                    || l["status"].to_string().replace(['{', '}', '"', ' '], ""),
                    str::to_string,
                ),
            )
        })
        .collect()
}

fn audit_verify_exit(f: &Fixture) -> (i32, String) {
    let out = f.fixture_cmd().args(["audit", "verify"]).output().unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
    )
}

fn spawn(f: &Fixture, root: &Path, env: &[(&str, &str)]) -> Pty {
    let mut cmd = f.command_for("sh");
    for (k, v) in env {
        cmd.env(k, v);
    }
    Pty::spawn_with(cmd, &bin_path(), &[root], &f.root().join("pty"), 30, 100)
}

/// Marks the first two rows, plans, opens the prompt and types `count`.
fn mark_two_and_confirm(p: &mut Pty, count: &str) {
    p.wait_for(SCAN_DONE);
    std::thread::sleep(Duration::from_millis(300));
    p.pump();
    p.keys(&[b" ", b"j", b" "]);
    std::thread::sleep(Duration::from_millis(300));
    p.pump();
    p.keys(&[b"p"]);
    p.wait_for("PLAN PREVIEW");
    let from = p.seen.len();
    p.keys(&[b"d"]);
    p.wait_for_after("Type 2 and press Enter to delete", from);
    p.keys(&[count.as_bytes(), b"\r"]);
}

fn run_child(name: &str) {
    if !unshare_works() {
        return;
    }
    let out = Command::new("unshare")
        .arg("-rm")
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD_VAR, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("CHILD_OK"),
        "the child {name} failed in the mount namespace:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn is_child() -> bool {
    std::env::var_os(CHILD_VAR).is_some()
}

const FINISHED: [&str; 7] = [
    "run_start",
    "explorer_confirm",
    "intent",
    "result",
    "intent",
    "result",
    "run_end",
];

#[test]
fn deleting_a_node_modules_and_a_plain_directory_should_leave_the_audit_trail_and_verify() {
    let f = Fixture::new();
    let root = tree(&f);
    write_config(&f, &root);
    let mut p = spawn(&f, &root, &[]);
    mark_two_and_confirm(&mut p, "2");
    p.wait_for("Freed ");
    p.keys(&[b"\x1b"]);
    p.send(b"q");
    assert_eq!(p.exit_code(), 0, "{}", p.tail());
    p.finish();
    p.assert_restored();
    assert!(!root.join("node_modules").exists());
    assert!(!root.join("plain").exists());
    let lines = audit_lines(&f);
    assert_eq!(types_of(&lines), FINISHED);
    assert_eq!(lines[1]["typed"], "2");
    assert_eq!(lines[1]["count"], 2);
    assert!(results(&lines).iter().all(|(_, s)| s == "removed"));
    assert_eq!(lines[6]["interrupted"], false);
    let outside = p.outside_alt();
    let closing = outside
        .lines()
        .find(|l| l.starts_with("EXPLORE CLOSED"))
        .unwrap_or_else(|| panic!("no closing line: {outside:?}"));
    assert!(closing.contains("deleted 2 items, "), "{closing}");
    assert!(closing.contains(" measured. Audit log: "), "{closing}");
    let (code, text) = audit_verify_exit(&f);
    assert_eq!(code, 0, "audit verify: {text}");
}

/// Inside `unshare -rm`: `holder` has a tmpfs mounted below it. The mount point and what is in it survive and
/// the directory is reported `Partial`; the plain directory is gone; the log verifies.
#[test]
fn a_directory_holding_a_nested_mount_should_end_partial_and_leave_the_mount_untouched() {
    if !is_child() {
        run_child(
            "a_directory_holding_a_nested_mount_should_end_partial_and_leave_the_mount_untouched",
        );
        return;
    }
    let f = Fixture::new();
    let code = f.home().join("code");
    dir_with(&code.join("plain"), 6, 6000);
    dir_with(&code.join("holder/own"), 2, 3000);
    let mnt = code.join("holder/mnt");
    fs::create_dir_all(&mnt).unwrap();
    let st = Command::new("mount")
        .args(["-t", "tmpfs", "tmpfs"])
        .arg(&mnt)
        .status()
        .unwrap();
    assert!(st.success(), "mount -t tmpfs");
    fs::write(mnt.join("keep"), b"mounted data").unwrap();
    let root = fs::canonicalize(&code).unwrap();
    write_config(&f, &root);
    let mut p = spawn(&f, &root, &[]);
    mark_two_and_confirm(&mut p, "2");
    p.wait_for("Freed ");
    p.keys(&[b"\x1b"]);
    p.send(b"q");
    assert_eq!(p.exit_code(), 0, "{}", p.tail());
    p.finish();
    p.assert_restored();
    assert!(!root.join("plain").exists(), "the plain directory is gone");
    assert_eq!(fs::read(mnt.join("keep")).unwrap(), b"mounted data");
    assert!(
        !root.join("holder/own").exists(),
        "the directory's own files are gone"
    );
    let lines = audit_lines(&f);
    assert_eq!(types_of(&lines), FINISHED);
    // The engine's Partial: the directory's own files went, the mount point stayed (`skipped: cross-device`).
    let by_path = results(&lines);
    let holder = by_path
        .iter()
        .find(|(p, _)| p.ends_with("/holder"))
        .expect("a result for the directory with the mount");
    assert_eq!(holder.1, "skipped:cross-device", "{by_path:?}");
    let plain = by_path.iter().find(|(p, _)| p.ends_with("/plain")).unwrap();
    assert_eq!(plain.1, "removed", "{by_path:?}");
    let (code, text) = audit_verify_exit(&f);
    assert_eq!(code, 0, "audit verify: {text}");
    println!("CHILD_OK");
}

/// SIGTERM while the delete runs (a test-support pause after each item holds it open): exit 143, the terminal
/// is restored, `run_end` says `interrupted`, the second directory is still there and the log verifies.
#[test]
fn sigterm_during_the_delete_should_exit_143_record_run_end_interrupted_and_restore() {
    let f = Fixture::new();
    let root = tree(&f);
    write_config(&f, &root);
    let mut p = spawn(&f, &root, &[("DC_EXPLORE_DELETE_DELAY_MS", "8000")]);
    mark_two_and_confirm(&mut p, "2");
    let log = f.xdg("state").join("dunnage/audit.jsonl");
    let end = std::time::Instant::now() + Duration::from_secs(20);
    while !fs::read_to_string(&log).is_ok_and(|t| t.contains("\"type\":\"result\"")) {
        assert!(
            std::time::Instant::now() < end,
            "no first result: {}",
            p.tail()
        );
        p.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    p.kill("TERM");
    assert_eq!(p.exit_code(), 143, "{}", p.tail());
    p.finish();
    p.assert_restored();
    let lines = audit_lines(&f);
    let types = types_of(&lines);
    assert_eq!(
        types.last().map(String::as_str),
        Some("run_end"),
        "{types:?}"
    );
    assert_eq!(
        lines.last().unwrap()["interrupted"],
        true,
        "{:?}",
        lines.last()
    );
    assert!(
        !root.join("node_modules").exists(),
        "the first directory was deleted before the signal"
    );
    assert!(
        root.join("plain").exists(),
        "the second directory was not touched"
    );
    let (code, text) = audit_verify_exit(&f);
    assert_eq!(code, 0, "audit verify: {text}");
}

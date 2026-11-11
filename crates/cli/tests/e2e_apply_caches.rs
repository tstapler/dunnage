#![cfg(feature = "gate-b")]
#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 2.3.3, Task 2.3.3a: Gate B, `--apply --kinds caches` end to end through the spawned binary under
//! `fixture_cmd()`. Run with `GATE=B cargo test --workspace --features disk-cleanup/gate-b`; a default build
//! refuses `--apply --kinds caches` by design, so this file is compiled only with the feature.
//!
//! Destructive-flow cases run on the test-only `fixture-destructive` kind (`stale-*` directories below the scan
//! roots); the real `build-dirs` variants are Story 3.2.4. Nothing here runs a real brew, go, pip, npm, docker,
//! apt or steam: the command rows meet shims under `<fixture>/bin` or are absent.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{aged_dir, next_commands, run_line, text_of, Fixture};

const CACHE_ROWS: [&str; 2] = [".npm/_cacache", ".cargo/registry/src"];

/// A cold fixture: two path caches and one stale directory under `~/code/proj`.
fn cold(f: &Fixture) -> PathBuf {
    for rel in CACHE_ROWS {
        let dir = f.home().join(rel);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/blob"), vec![3u8; 20_000]).unwrap();
    }
    let proj = f.home().join("code/proj");
    aged_dir(&proj.join("stale-a"), 9_000, 90);
    proj
}

fn plain(f: &Fixture, args: &[&str]) -> std::process::Output {
    f.fixture_cmd()
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn caches_gone(f: &Fixture) -> bool {
    CACHE_ROWS.iter().all(|r| !f.home().join(r).exists())
}

// ------------------------------------------------------------------ registered kinds

#[test]
fn kinds_should_list_caches_as_available_and_the_other_gates_as_not_available_When_the_gate_b_build_runs(
) {
    let f = Fixture::new();
    let out = plain(&f, &["kinds", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let cell = |id: &str| -> String {
        doc["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["id"] == id)
            .unwrap_or_else(|| panic!("no kind {id}"))["available"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(cell("caches"), "yes");
    let euid_is_root = Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|o| o.stdout == b"0\n");
    assert_eq!(
        cell("cache-apt"),
        if euid_is_root { "yes" } else { "needs root" }
    );
    for id in [
        "worktrees",
        "build-dirs",
        "docker",
        "docker-images",
        "docker-volumes",
        "steam",
        "btrfs",
        "btrfs-trim",
    ] {
        assert_eq!(cell(id), "not available in this build", "{id}");
        for apply in [false, true] {
            let mut args = vec!["--kinds", id];
            if apply {
                args.insert(0, "--apply");
            }
            let out = plain(&f, &args);
            assert_eq!(out.status.code(), Some(64), "{args:?}");
            assert!(out.stdout.is_empty(), "{args:?}");
            assert!(
                text_of(&out).1.contains(&format!(
                    "'{id}' is not available in this build (see 'disk-cleanup kinds')"
                )),
                "{args:?}: {}",
                text_of(&out).1
            );
        }
    }
}

// ------------------------------------------------------------------ the two-command flow

#[test]
fn the_two_command_flow_should_remove_the_caches_with_no_prompt_and_apply_the_destructive_line_only_with_its_id(
) {
    // First command: the printed `disk-cleanup --apply` line removes the caches.
    let f = Fixture::new();
    let proj = cold(&f);
    let dry = plain(&f, &[]);
    assert_eq!(dry.status.code(), Some(0));
    let cmds = next_commands(&text_of(&dry).0);
    assert_eq!(cmds.len(), 2, "{cmds:?}\n{}", text_of(&dry).0);
    assert_eq!(cmds[0], "disk-cleanup --apply");
    assert!(!cmds[0].contains("--plan-id"));
    let out = run_line(&f, &cmds[0]);
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    assert!(caches_gone(&f), "the first Next line left a cache behind");
    assert!(
        proj.join("stale-a").is_dir(),
        "the safe line touched the stale directory"
    );

    // Second command, on a cold fixture: the line that adds fixture-destructive carries --plan-id.
    let f = Fixture::new();
    let proj = cold(&f);
    let dry = plain(&f, &[]);
    let cmds = next_commands(&text_of(&dry).0);
    let with_id = &cmds[1];
    assert!(with_id.contains("--plan-id"), "{with_id}");
    assert!(with_id.contains("fixture-destructive"), "{with_id}");
    assert!(
        !with_id.contains("worktrees"),
        "the echoed line names a kind this build does not ship: {with_id}"
    );
    // The id altered by one hex digit: exit 64, nothing deleted.
    let (head, id) = with_id.rsplit_once(' ').unwrap();
    let mut digits: Vec<char> = id.chars().collect();
    digits[0] = if digits[0] == '0' { '1' } else { '0' };
    let altered = format!("{head} {}", digits.into_iter().collect::<String>());
    let out = run_line(&f, &altered);
    assert_eq!(out.status.code(), Some(64), "{}", text_of(&out).1);
    assert!(out.stdout.is_empty());
    assert!(proj.join("stale-a").is_dir());
    assert!(!caches_gone(&f));
    // The printed line itself applies, with stdin from /dev/null.
    let out = run_line(&f, with_id);
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    assert!(
        !proj.join("stale-a").exists(),
        "the stale directory is still there"
    );
    assert!(caches_gone(&f));
}

// ------------------------------------------------------------------ a later dry run does not clobber the id

/// Rewrites `saved_at` of the `last_plan` entry `id` to `age_secs` before now (the binary has no injectable
/// clock, so the fixture clock "moves" by moving the saved entry back).
fn age_saved_plan(f: &Fixture, id: &str, age_secs: i64) {
    let path = f.xdg("state").join("disk-cleanup/last_plan");
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    let entries = doc["entries"].as_array_mut().unwrap();
    let entry = entries
        .iter_mut()
        .find(|e| e["plan_id"] == id)
        .unwrap_or_else(|| panic!("no saved entry {id}"));
    entry["saved_at"] = serde_json::json!(now - age_secs);
    std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
}

fn fixture_id(f: &Fixture) -> String {
    let out = plain(f, &["--kinds", "fixture-destructive", "--json"]);
    common::extract_plan_id(&text_of(&out).0).expect("plan id")
}

fn apply_fixture_kind(f: &Fixture, id: &str) -> std::process::Output {
    plain(
        f,
        &["--apply", "--kinds", "fixture-destructive", "--plan-id", id],
    )
}

#[test]
fn a_later_dry_run_should_not_clobber_the_id_the_owner_copied_and_a_stale_id_should_be_refused() {
    let f = Fixture::new();
    let proj = cold(&f);
    let a = fixture_id(&f);
    // An agent's dry runs: --json and the caches-only plan.
    assert_eq!(plain(&f, &["--json"]).status.code(), Some(0));
    assert_eq!(plain(&f, &["--kinds", "caches"]).status.code(), Some(0));
    let out = apply_fixture_kind(&f, &a);
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    assert!(!proj.join("stale-a").exists());

    // 23 h old: still accepted.
    let f = Fixture::new();
    let proj = cold(&f);
    let id = fixture_id(&f);
    age_saved_plan(&f, &id, 23 * 3600);
    let out = apply_fixture_kind(&f, &id);
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    assert!(!proj.join("stale-a").exists());

    // 25 h old: refused as stale, nothing deleted.
    let f = Fixture::new();
    let proj = cold(&f);
    let id = fixture_id(&f);
    age_saved_plan(&f, &id, 25 * 3600);
    let out = apply_fixture_kind(&f, &id);
    assert_eq!(out.status.code(), Some(64), "{}", text_of(&out).1);
    assert!(out.stdout.is_empty());
    assert!(
        text_of(&out).1.contains("24"),
        "the refusal does not name the 24 h limit: {}",
        text_of(&out).1
    );
    assert!(proj.join("stale-a").is_dir());
}

// ------------------------------------------------------------------ a pasted Next line reproduces the plan

fn mixed_stale(f: &Fixture) {
    let proj = f.home().join("code/proj");
    aged_dir(&proj.join("stale-old-big"), 65_536, 90);
    aged_dir(&proj.join("stale-old-small"), 10, 90);
    aged_dir(&proj.join("stale-young"), 20_000, 1);
    aged_dir(&f.home().join("work/stale-w"), 20_000, 90);
}

fn exists(f: &Fixture, rel: &str) -> bool {
    f.home().join(rel).exists()
}

struct Case {
    /// Flags of the dry run, after `--kinds caches,fixture-destructive`.
    flags: &'static [&'static str],
    /// Directories (relative to HOME) the apply must remove; every other stale directory must stay.
    removed: &'static [&'static str],
    /// The flag (with its value) to drop from the pasted line, `None` when the case has no such flag on it.
    drop: Option<&'static str>,
}

const STALE: [&str; 4] = [
    "code/proj/stale-old-big",
    "code/proj/stale-old-small",
    "code/proj/stale-young",
    "work/stale-w",
];

fn run_case(case: &Case, work_root: bool) {
    let mut flags: Vec<String> = case.flags.iter().map(ToString::to_string).collect();
    let f = Fixture::new();
    mixed_stale(&f);
    if work_root {
        flags.push(f.home().join("work").to_string_lossy().into_owned());
    }
    let mut dry_args = vec![
        "--kinds".to_string(),
        "caches,fixture-destructive".to_string(),
    ];
    dry_args.extend(flags.clone());
    let dry = f
        .fixture_cmd()
        .args(&dry_args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        dry.status.code(),
        Some(0),
        "{dry_args:?}: {}",
        text_of(&dry).1
    );
    let cmds = next_commands(&text_of(&dry).0);
    for c in &cmds {
        let sh = Command::new("sh").args(["-n", "-c", c]).output().unwrap();
        assert!(sh.status.success(), "`sh -n` rejects the printed line: {c}");
    }
    let with_id = cmds
        .iter()
        .find(|c| c.contains("--plan-id"))
        .unwrap_or_else(|| panic!("no Next line carries --plan-id: {cmds:?}"))
        .clone();

    if let Some(flag) = case.drop {
        let tokens: Vec<&str> = with_id.split_whitespace().collect();
        let at = tokens
            .iter()
            .position(|t| *t == flag)
            .unwrap_or_else(|| panic!("the printed line lacks {flag}: {with_id}"));
        let mut rest = tokens.clone();
        rest.drain(at..at + 2);
        let out = run_line(&f, &rest.join(" "));
        assert_eq!(
            out.status.code(),
            Some(64),
            "{flag} dropped: {}",
            text_of(&out).1
        );
        assert!(out.stdout.is_empty());
        assert!(
            text_of(&out).1.to_lowercase().contains("changed"),
            "{flag} dropped, not the `changed` text: {}",
            text_of(&out).1
        );
        for s in STALE {
            assert!(exists(&f, s), "{s} was deleted by the refused line");
        }
    }
    let out = run_line(&f, &with_id);
    assert_eq!(out.status.code(), Some(0), "{with_id}: {}", text_of(&out).1);
    for s in STALE {
        assert_eq!(
            exists(&f, s),
            !case.removed.contains(&s),
            "{s} after `{with_id}`"
        );
    }
}

#[test]
fn a_pasted_next_line_should_apply_exactly_the_reviewed_free_rows_and_refuse_the_line_with_one_flag_removed(
) {
    run_case(
        &Case {
            flags: &["--min-age", "60d"],
            removed: &["code/proj/stale-old-big", "code/proj/stale-old-small"],
            drop: Some("--min-age"),
        },
        false,
    );
    run_case(
        &Case {
            // 0 is what the test kind does without the flag, so dropping it changes nothing the gate compares.
            flags: &["--keep-newest", "0"],
            removed: &STALE[..3],
            drop: None,
        },
        false,
    );
    run_case(
        &Case {
            flags: &["--keep-newest", "1"],
            removed: &["code/proj/stale-old-big", "code/proj/stale-old-small"],
            drop: Some("--keep-newest"),
        },
        false,
    );
    run_case(
        &Case {
            flags: &["--min-size", "8KiB"],
            removed: &["code/proj/stale-old-big", "code/proj/stale-young"],
            drop: Some("--min-size"),
        },
        false,
    );
    run_case(
        &Case {
            flags: &["--root"],
            removed: &["work/stale-w"],
            drop: Some("--root"),
        },
        true,
    );
}

#[test]
fn a_pasted_next_line_should_carry_the_resolved_kinds_When_except_removes_one_of_a_multi_kind_selection(
) {
    let f = Fixture::new();
    mixed_stale(&f);
    cold(&f);
    let dry = plain(
        &f,
        &[
            "--kinds",
            "caches,fixture-destructive",
            "--except",
            "caches",
        ],
    );
    assert_eq!(dry.status.code(), Some(0), "{}", text_of(&dry).1);
    let cmds = next_commands(&text_of(&dry).0);
    let with_id = cmds.iter().find(|c| c.contains("--plan-id")).unwrap();
    assert!(with_id.contains("--kinds fixture-destructive"), "{with_id}");
    let out = run_line(&f, with_id);
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    // `work/` is outside the default scan roots (`~/code`), so only the `code/proj` directories are in scope.
    for s in &STALE[..3] {
        assert!(!exists(&f, s), "{s}");
    }
    assert!(exists(&f, STALE[3]));
    assert!(!caches_gone(&f), "--except caches still removed a cache");
}

// ------------------------------------------------------------------ JSON of a real apply

#[test]
fn a_json_apply_should_carry_fs_after_the_per_item_bytes_and_the_exit_code_and_a_plan_should_not() {
    let f = Fixture::new();
    cold(&f);
    let plan = plain(&f, &["--kinds", "caches", "--json"]);
    let plan: serde_json::Value = serde_json::from_slice(&plan.stdout).unwrap();
    assert!(plan["fs"]["after"].is_null(), "plan mode has no fs.after");
    assert_eq!(plan["mode"], "plan");

    let out = plain(&f, &["--apply", "--kinds", "caches", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON document");
    assert_eq!(doc["exit_code"], 0);
    assert!(doc["fs"]["after"].is_object(), "{}", doc["fs"]);
    let items = doc["items"].as_array().unwrap();
    let removed: Vec<&serde_json::Value> =
        items.iter().filter(|i| i["status"] == "freed").collect();
    assert_eq!(removed.len(), 2, "{items:#?}");
    for i in &removed {
        assert!(i["est_bytes"].as_u64().unwrap() > 0);
        assert!(i["measured_bytes"].as_u64().unwrap() > 0);
        assert!(i["elapsed_ms"].is_u64());
    }
    assert!(caches_gone(&f));
}

// ------------------------------------------------------------------ after an apply, audit verify is clean

#[test]
fn audit_verify_should_report_zero_deletions_outside_the_allowed_roots_After_a_cache_apply() {
    let f = Fixture::new();
    cold(&f);
    assert_eq!(
        plain(&f, &["--apply", "--kinds", "caches"]).status.code(),
        Some(0)
    );
    let out = plain(&f, &["audit", "verify"]);
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    let (stdout, _) = text_of(&out);
    assert!(
        stdout.contains("2 deletions in 1 runs, 0 outside the allowed roots"),
        "{stdout}"
    );
}

// ------------------------------------------------------------------ no network

fn strace_network(f: &Fixture, log: &Path, args: &[&str]) -> std::process::Output {
    let strace = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join("strace"))
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("strace"));
    f.command_for(strace)
        .args(["-f", "-e", "trace=network", "-o"])
        .arg(log)
        .arg(env!("CARGO_BIN_EXE_disk-cleanup"))
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn the_plan_and_the_apply_should_open_no_inet_socket_When_traced_and_the_plan_should_exit_0_in_a_network_namespace(
) {
    if !common::strace_available() {
        assert!(
            std::env::var_os("DC_REQUIRE_STRACE").is_none(),
            "DC_REQUIRE_STRACE is set"
        );
        eprintln!("SKIP: strace is not installed");
        return;
    }
    let f = Fixture::new();
    cold(&f);
    for (name, args) in [
        ("plan", vec!["--kinds", "caches"]),
        ("apply", vec!["--apply", "--kinds", "caches"]),
    ] {
        let log = f.root().join(format!("net-{name}.log"));
        let out = strace_network(&f, &log, &args);
        assert_eq!(out.status.code(), Some(0), "{name}: {}", text_of(&out).1);
        let trace = std::fs::read_to_string(&log).unwrap();
        for bad in ["AF_INET", "AF_INET6", "AF_NETLINK", "AF_PACKET"] {
            // AF_UNIX (the Docker socket) is the only family allowed.
            assert!(
                !trace
                    .lines()
                    .any(|l| l.contains("socket(") && l.contains(bad)),
                "{name}: {bad} in the trace:\n{trace}"
            );
        }
        assert!(
            !trace.lines().any(|l| l.contains("connect(")
                && (l.contains("AF_INET") || l.contains("sin_addr") || l.contains("sin6_addr"))),
            "{name}: an inet connect in the trace:\n{trace}"
        );
    }
    // Where `unshare -rn` works, the plan still exits 0 with only a down loopback.
    if Command::new("unshare")
        .args(["-rn", "true"])
        .output()
        .is_ok_and(|o| o.status.success())
    {
        let f = Fixture::new();
        cold(&f);
        let out = f
            .command_for("unshare")
            .args(["-rn"])
            .arg(env!("CARGO_BIN_EXE_disk-cleanup"))
            .args(["--kinds", "caches"])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
        assert!(text_of(&out).0.contains("DRY RUN"));
    }
}

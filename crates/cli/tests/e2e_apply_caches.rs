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

use std::os::unix::fs::PermissionsExt;
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
        if cfg!(feature = "gate-c") && (id == "worktrees" || id == "build-dirs") {
            // Gate C registers them (Stories 3.1.3 and 3.2.4); `worktree_git.rs` and
            // `e2e_apply_build_dirs.rs` cover them.
            continue;
        }
        if cfg!(feature = "gate-d") && id == "steam" {
            // Gate D registers it (Story 4.2.2); `e2e_apply_steam.rs` covers it.
            continue;
        }
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
        cfg!(feature = "gate-c") || !with_id.contains("worktrees"),
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

// ------------------------------------------------------------------ apply-mode canary and subprocess confinement

const SYSTEM_PREFIXES: [&str; 8] = [
    "/usr",
    "/lib",
    "/lib64",
    "/proc/self",
    "/sys/fs/btrfs",
    "/sys/fs/cgroup",
    "/dev",
    "/bin",
];
/// `/sys/devices/system/cpu/online`: the walker of the Gate C build-dirs discovery sizes its thread pool from it.
const SYSTEM_EXACT: [&str; 3] = [
    "/etc/ld.so.cache",
    "/etc/ld.so.preload",
    "/sys/devices/system/cpu/online",
];

/// `<fixture>/bin/go`: answers `go env GOCACHE` with a directory below the fixture HOME and empties it on
/// `go clean -cache`, logging every call.
fn go_shim(f: &Fixture) -> PathBuf {
    let bin = f.root().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    common::docker_down_shim(&bin);
    let shim = bin.join("go");
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> '{log}'\n\
         case \"$1 $2\" in\n\
         'env GOCACHE') echo \"{cache}\" ;;\n\
         'clean -cache') rm -rf \"{cache}\"/* ;;\n\
         esac\n",
        log = f.root().join("shim-go.log").display(),
        cache = f.home().join(".cache/go-build").display(),
    );
    std::fs::write(&shim, script).unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cache = f.home().join(".cache/go-build/ab");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("obj"), vec![1u8; 30_000]).unwrap();
    shim
}

fn fixture_path(f: &Fixture) -> std::ffi::OsString {
    std::env::join_paths([
        f.root().join("bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ])
    .unwrap()
}

#[test]
fn safety_fixture_home_canary() {
    if !common::strace_available() {
        assert!(
            std::env::var_os("DC_REQUIRE_STRACE").is_none(),
            "DC_REQUIRE_STRACE is set"
        );
        eprintln!("SKIP: strace is not installed (DC_REQUIRE_STRACE=1 makes the canary a failure)");
        return;
    }
    let f = Fixture::new();
    cold(&f);
    let shim = go_shim(&f);
    let log = f.root().join("apply.log");
    let mut c = common::strace_cmd(&f, &log, env!("CARGO_BIN_EXE_disk-cleanup"));
    c.args(["--apply", "--kinds", "caches"])
        .env("PATH", fixture_path(&f))
        .current_dir(f.home())
        .stdin(std::process::Stdio::null());
    let out = c.output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);

    // The run really removed the path caches and ran the shim (the trace is not of a no-op).
    assert!(caches_gone(&f));
    let calls = std::fs::read_to_string(f.root().join("shim-go.log")).unwrap();
    assert!(
        calls.contains("env GOCACHE") && calls.contains("clean -cache"),
        "{calls}"
    );

    let paths = common::strace_own_paths(&std::fs::read_to_string(&log).unwrap());
    assert!(
        paths
            .iter()
            .any(|p| p.starts_with(f.root().to_str().unwrap())),
        "vacuous trace: {} paths, none under the fixture",
        paths.len()
    );
    let mut allowed: Vec<PathBuf> = SYSTEM_PREFIXES.iter().map(PathBuf::from).collect();
    allowed.push(std::fs::canonicalize(f.root()).unwrap());
    allowed.push(f.root().to_path_buf());
    allowed.push(
        Path::new(env!("CARGO_BIN_EXE_disk-cleanup"))
            .parent()
            .unwrap()
            .to_path_buf(),
    );
    let ancestors: Vec<String> = std::fs::canonicalize(f.root())
        .unwrap()
        .ancestors()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let exact: Vec<&str> = SYSTEM_EXACT
        .iter()
        .copied()
        .chain(ancestors.iter().map(String::as_str))
        .collect();
    // Snapshot detection lstats `<mount>/.snapshots` for each mount: a read-only probe, not a home access.
    let bad: Vec<String> = common::canary_violations(&paths, &allowed, &exact)
        .into_iter()
        .filter(|p| !p.ends_with("/.snapshots") && p != "/timeshift-btrfs/snapshots")
        .collect();
    assert!(
        bad.is_empty(),
        "paths outside the fixture and the allowlist: {bad:#?}"
    );

    // The Go row ran the shim and no system `go`: the trace shows the execve of the shim by absolute path. The
    // audit `intent` records the argv as `go clean -cache` (the program name, not the resolved path).
    let trace = std::fs::read_to_string(&log).unwrap();
    let execs: Vec<&str> = trace.lines().filter(|l| l.contains("execve(")).collect();
    assert!(
        execs
            .iter()
            .any(|l| l.contains(&format!("execve(\"{}\"", shim.display()))),
        "no execve of the shim {}: {execs:#?}",
        shim.display()
    );
    assert!(
        !execs.iter().any(|l| l.contains("/usr/bin/go\"")
            || l.contains("/bin/go\"") && !l.contains(f.root().to_str().unwrap())),
        "a system go was executed: {execs:#?}"
    );
    let audit = std::fs::read_to_string(f.xdg("state").join("disk-cleanup/audit.jsonl")).unwrap();
    let intents: Vec<serde_json::Value> = audit
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["type"] == "intent" && v["argv"].is_array())
        .collect();
    assert_eq!(intents.len(), 1, "{audit}");
    assert_eq!(
        intents[0]["argv"],
        serde_json::json!(["go", "clean", "-cache"])
    );
}

// ------------------------------------------------------------------ ENOSPC mid-run (UX-18), in process

mod enospc {
    use std::ffi::OsString;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use disk_cleanup_adapters::targets::cache::CacheTarget;
    use disk_cleanup_adapters::testing::FaultDirOps;
    use disk_cleanup_adapters::RealDirOps;
    use disk_cleanup_cli::app::{ApplyMode, Ports};
    use disk_cleanup_cli::terminal::Terminal;
    use disk_cleanup_cli::wiring::audit::{GateExit, StampOutcome};
    use disk_cleanup_cli::wiring::compose::{compose_apply, ApplyState, ApplyWiring, Sink, Trace};
    use disk_cleanup_core::ports::Env;
    use disk_cleanup_core::roots::RootSet;
    use disk_cleanup_core::testing::{
        FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs, FakePlanStore, FakeProbe,
        FakeRunGuard, FakeSpaceMeter,
    };
    use disk_cleanup_core::{
        ApplyToken, AuditRecord, BuildInfo, Dirs, FatalError, Identity, RunRoots, Status,
    };

    use super::common::Fixture;

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct NoState;

    impl ApplyState for NoState {
        fn unclean_notice(&self) -> Result<Option<String>, String> {
            Ok(None)
        }

        fn ensure_reserves(&self) -> Result<Option<String>, FatalError> {
            Ok(None)
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn confined(_gate: char) -> Result<StampOutcome, GateExit> {
        Ok(StampOutcome::Confined)
    }

    fn canon(p: &std::path::Path) -> std::io::Result<(PathBuf, Identity)> {
        let c = p.canonicalize()?;
        let m = std::fs::metadata(&c)?;
        Ok((c.clone(), Identity::of(&m)))
    }

    /// Four caches of distinct sizes (so the plan order is known) under a fixture HOME; the `n`-th `unlinkat` of
    /// the run fails with `ENOSPC`. Returns (exit code, stdout, stderr, audit results).
    pub fn run(
        f: &Fixture,
        fail_on_unlink: usize,
    ) -> (i32, String, String, Vec<(PathBuf, Status)>) {
        let home = std::fs::canonicalize(f.home()).unwrap();
        for (rel, kib) in [
            (".npm/_cacache", 400),
            (".cargo/registry/src", 300),
            (".cache/ms-playwright", 200),
            (".gradle/caches", 100),
        ] {
            let dir = home.join(rel);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("blob"), vec![4u8; kib * 1024]).unwrap();
        }
        std::fs::create_dir_all(home.join("code")).unwrap();
        let dirs = Dirs {
            home: home.clone(),
            state: home.join(".local/state/disk-cleanup"),
            cache: home.join(".cache/disk-cleanup"),
            config: home.join(".config/disk-cleanup"),
        };
        let roots = RootSet::resolve(&dirs, &[home.join("code")], None, &home, &canon).unwrap();
        let target = CacheTarget::new(
            roots,
            home.join("fsroot"),
            FaultDirOps::wrap(RealDirOps::confined_to(&home))
                .enospc_on_nth_unlinkat(fail_on_unlink)
                .build(),
        );
        let runner = FakeCommandRunner::new();
        runner.absent("go").absent("pip").absent("pip3");
        let (fs, clock, probe) = (
            FakeFs::new(),
            FakeClock::at(1_800_000_000),
            FakeProbe::new(),
        );
        let space = FakeSpaceMeter::new(
            vec![FakeSpaceMeter::plain_snapshot("/", 500, 1000)],
            FakeSpaceMeter::stable(),
        );
        let (audit, guard, plans) = (
            FakeAuditSink::new(),
            FakeRunGuard::new(),
            FakePlanStore::new(),
        );
        let env = Env {
            fs: &fs,
            runner: &runner,
            clock: &clock,
            probe: &probe,
            space: &space,
            audit: &audit,
            guard: &guard,
            plans: &plans,
            dirs,
        };
        let mint = ApplyToken::for_tests;
        let outcome = |_m, _v, _f, _u| {};
        let ports = Ports {
            env: &env,
            targets: vec![&target],
            apply: ApplyMode::Granted(&mint),
            apply_capable: None,
            stamp: &confined,
            build: BuildInfo {
                bin_version: "0.1.0".into(),
                git_commit: "abc".into(),
                dirty: false,
                metascope_rev: "none".into(),
                patched: false,
                metascope_dirty: false,
            },
            roots: RunRoots::default(),
            scan_roots: 1,
            exe: PathBuf::from("/usr/bin/disk-cleanup"),
            term: Terminal::pipe(),
            stdout_tty: false,
            run_id: "run-1".into(),
            via: None,
            outcome: &outcome,
            reserves_note: None,
            config_roots: None,
        };
        let (out, err) = (Shared::default(), Shared::default());
        let mut trace = Trace::default();
        let argv: Vec<OsString> = ["--apply", "--kinds", "caches", "--jobs", "1"]
            .iter()
            .map(OsString::from)
            .collect();
        let code = compose_apply(
            argv,
            &ApplyWiring {
                ports: &ports,
                state: &NoState,
                stderr_tty: false,
            },
            &Sink::new(out.clone()),
            &Sink::new(err.clone()),
            &mut trace,
        );
        let results = audit
            .records()
            .iter()
            .filter_map(|r| match r {
                AuditRecord::Result(i) => Some((i.0.path.clone().unwrap(), i.0.status.clone())),
                _ => None,
            })
            .collect();
        let text = |s: &Shared| String::from_utf8(s.0.lock().unwrap().clone()).unwrap();
        (code, text(&out), text(&err), results)
    }
}

#[test]
fn an_enospc_while_removing_the_second_cache_should_leave_the_first_removed_the_rest_skipped_and_exit_2(
) {
    let f = Fixture::new();
    // Each cache is one file plus the directory: the third `unlinkat` is the first one of the second cache.
    let (code, stdout, stderr, results) = enospc::run(&f, 3);
    assert_eq!(code, 2, "{stdout}\n{stderr}");
    let home = std::fs::canonicalize(f.home()).unwrap();
    // Plan order is the table's: npm first, gradle second, then cargo src and playwright.
    assert!(
        !home.join(".npm/_cacache").exists(),
        "the first cache was not removed"
    );
    assert!(
        home.join(".gradle/caches/blob").exists(),
        "the failed cache lost its file"
    );
    assert!(home.join(".cargo/registry/src/blob").exists());
    assert!(home.join(".cache/ms-playwright/blob").exists());
    let status_of = |rel: &str| {
        results
            .iter()
            .find(|(p, _)| p == &home.join(rel))
            .map(|(_, s)| s.clone())
    };
    assert_eq!(
        status_of(".npm/_cacache"),
        Some(disk_cleanup_core::Status::Removed)
    );
    assert!(
        matches!(status_of(".gradle/caches"), Some(disk_cleanup_core::Status::Error(m)) if m.contains("no space left on device")),
        "{results:?}"
    );
    assert_eq!(
        status_of(".cargo/registry/src"),
        None,
        "attempted after the abort"
    );
    assert_eq!(
        status_of(".cache/ms-playwright"),
        None,
        "attempted after the abort"
    );
    assert!(
        stdout.contains("INTERRUPTED: 2 items not attempted"),
        "{stdout}"
    );
    assert!(
        stdout.contains("btrfs metadata/unallocated is the limit")
            && stdout.contains("disk-cleanup status"),
        "no recovery hint:\n{stdout}"
    );
    assert!(stdout.ends_with("APPLY FINISHED - exit 2.\n"), "{stdout}");
}

// ------------------------------------------------------------------ full state directory (rehearsal script)

mod full_disk {
    use super::{cold, common, text_of, Fixture};
    use std::process::{Command, Output};

    /// The command prefix that runs the binary under the fixture environment inside the rehearsal shell,
    /// where `XDG_STATE_HOME` is the full tmpfs.
    pub fn fx(f: &Fixture) -> String {
        format!(
            "env -i PATH=/usr/bin:/bin HOME={} XDG_STATE_HOME=\"$XDG_STATE_HOME\" XDG_CACHE_HOME={} XDG_CONFIG_HOME={} DC_TEST_ROOT={} LANG=C {}",
            f.home().display(),
            f.xdg("cache").display(),
            f.xdg("config").display(),
            f.root().display(),
            env!("CARGO_BIN_EXE_disk-cleanup"),
        )
    }

    /// `scripts/rehearse-full-disk.sh <mode> -- sh -c <inner>`; `mode` is `--prime "<fx> status"` or
    /// `--no-reserve`. `None` when `unshare -rm` does not work here.
    pub fn rehearse(f: &Fixture, mode: &[&str], inner: &str) -> Option<Output> {
        if !common::unshare_works() {
            return None;
        }
        let script = common::repo_root().join("scripts/rehearse-full-disk.sh");
        let out = Command::new(script)
            .args(mode)
            .args(["--", "sh", "-c", inner])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            text_of(&out).1.contains("is full"),
            "the rehearsal did not fill the state dir: {}",
            text_of(&out).1
        );
        let _ = f;
        Some(out)
    }

    pub fn prime(f: &Fixture) -> String {
        format!("{} status >/dev/null 2>&1", fx(f))
    }

    /// `key=value` lines the inner script echoed.
    pub fn kv(out: &Output, key: &str) -> Option<String> {
        text_of(out)
            .0
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")).map(str::to_string))
    }

    pub fn fixture_with_stale() -> Fixture {
        let f = Fixture::new();
        cold(&f);
        f
    }
}

#[test]
fn a_full_state_dir_with_the_reserves_should_release_the_audit_reserve_and_still_delete_with_exit_0(
) {
    let f = full_disk::fixture_with_stale();
    let inner = format!(
        "{fx} --apply --kinds caches >\"{root}/apply.out\" 2>\"{root}/apply.err\"; echo rc=$?; \
         echo audit_reserve=$(stat -c %s \"$XDG_STATE_HOME/disk-cleanup/audit.reserve\"); \
         echo audit_lines=$(wc -l < \"$XDG_STATE_HOME/disk-cleanup/audit.jsonl\")",
        fx = full_disk::fx(&f),
        root = f.root().display()
    );
    let prime = full_disk::prime(&f);
    let Some(out) = full_disk::rehearse(&f, &["--prime", &prime], &inner) else {
        return;
    };
    let (stdout, stderr) = text_of(&out);
    assert_eq!(
        full_disk::kv(&out, "rc").as_deref(),
        Some("0"),
        "{stdout}\n{stderr}"
    );
    assert!(caches_gone(&f), "nothing was deleted\n{stdout}\n{stderr}");
    assert_eq!(
        full_disk::kv(&out, "audit_reserve").as_deref(),
        Some("0"),
        "the audit reserve was not released:\n{stdout}"
    );
    assert!(
        full_disk::kv(&out, "audit_lines")
            .unwrap()
            .parse::<u32>()
            .unwrap()
            >= 4,
        "{stdout}"
    );
}

#[test]
fn a_full_state_dir_without_the_reserves_should_exit_2_and_delete_nothing() {
    let f = full_disk::fixture_with_stale();
    let inner = format!(
        "{fx} --apply --kinds caches >\"{root}/apply.out\" 2>\"{root}/apply.err\"; echo rc=$?",
        fx = full_disk::fx(&f),
        root = f.root().display()
    );
    let Some(out) = full_disk::rehearse(&f, &["--no-reserve"], &inner) else {
        return;
    };
    let (stdout, stderr) = text_of(&out);
    assert_eq!(
        full_disk::kv(&out, "rc").as_deref(),
        Some("2"),
        "{stdout}\n{stderr}"
    );
    assert!(
        !caches_gone(&f) && f.home().join(".npm/_cacache/sub/blob").exists(),
        "a cache was deleted"
    );
}

#[test]
fn the_owner_flow_on_a_full_state_dir_should_save_the_id_and_apply_with_the_reserve_and_say_not_usable_without_it(
) {
    let dry_and_apply = |f: &Fixture| {
        format!(
            "{fx} --kinds caches,fixture-destructive >{root}/dry.out 2>{root}/dry.err; echo dry_rc=$?; \
             id=$(grep -o -- '--plan-id [0-9a-f]*' {root}/dry.out | head -1 | cut -d' ' -f2); \
             if [ -z \"$id\" ]; then id=$(sed -n 's/.*plan-id \\([0-9a-f]\\{{8\\}}\\).*/\\1/p' {root}/dry.out | head -1); fi; \
             {fx} --apply --kinds caches,fixture-destructive --plan-id \"$id\" >{root}/apply.out 2>{root}/apply.err </dev/null; echo apply_rc=$?",
            fx = full_disk::fx(f),
            root = f.root().display()
        )
    };

    // With the reserve: the id is saved, the printed line is accepted and applies.
    let f = full_disk::fixture_with_stale();
    let prime = full_disk::prime(&f);
    let Some(out) = full_disk::rehearse(&f, &["--prime", &prime], &dry_and_apply(&f)) else {
        return;
    };
    assert_eq!(
        full_disk::kv(&out, "dry_rc").as_deref(),
        Some("0"),
        "{}",
        text_of(&out).1
    );
    assert_eq!(
        full_disk::kv(&out, "apply_rc").as_deref(),
        Some("0"),
        "{}\n{}",
        text_of(&out).0,
        std::fs::read_to_string(f.root().join("apply.err")).unwrap_or_default()
    );
    let dry = std::fs::read_to_string(f.root().join("dry.out")).unwrap();
    assert!(
        !dry.contains("NOT usable") && !dry.contains("NOT saved"),
        "{dry}"
    );
    assert!(
        !f.home().join("code/proj/stale-a").exists(),
        "the destructive apply did not run"
    );

    // Without it: the report says the id is not usable, exits 0, and the destructive apply is refused.
    let f = full_disk::fixture_with_stale();
    let Some(out) = full_disk::rehearse(&f, &["--no-reserve"], &dry_and_apply(&f)) else {
        return;
    };
    assert_eq!(full_disk::kv(&out, "dry_rc").as_deref(), Some("0"));
    let dry = std::fs::read_to_string(f.root().join("dry.out")).unwrap();
    assert!(dry.contains("plan-id NOT usable"), "{dry}");
    let apply_rc = full_disk::kv(&out, "apply_rc").unwrap();
    let err = std::fs::read_to_string(f.root().join("apply.err")).unwrap_or_default();
    assert_ne!(
        apply_rc, "0",
        "a destructive apply ran with an unsaved id\n{err}"
    );
    assert_eq!(apply_rc, "64");
    assert!(
        err.contains("The state directory") && err.contains("free about 1 MiB there"),
        "{err}"
    );
    assert!(
        f.home().join("code/proj/stale-a").is_dir(),
        "the stale directory was deleted"
    );
}

// ------------------------------------------------------------------ an unconfined apply needs a valid gate stamp

/// The release Gate B binary, built in the private target dir `kind_registry.rs` uses, or `None` when
/// `DC_SKIP_PROFILE_BUILDS` is set.
fn release_gate_b() -> Option<PathBuf> {
    if std::env::var_os("DC_SKIP_PROFILE_BUILDS").is_some() {
        eprintln!("SKIP: DC_SKIP_PROFILE_BUILDS is set");
        return None;
    }
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let target = common::repo_root().join("target/kind-registry-builds");
    let out = Command::new(cargo)
        .current_dir(common::repo_root())
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env("CARGO_TARGET_DIR", &target)
        .args([
            "build",
            "--locked",
            "-p",
            "disk-cleanup",
            "--release",
            "--features",
            "gate-b",
        ])
        .output()
        .expect("cargo");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(target.join("release/disk-cleanup"))
}

fn sha256_of(path: &Path) -> String {
    let out = Command::new("sha256sum").arg(path).output().unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

fn git_head() -> String {
    let out = Command::new("git")
        .current_dir(common::repo_root())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn write_stamp(f: &Fixture, commit: &str, sha: &str) {
    let dir = f.xdg("state").join("disk-cleanup");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("gate-stamp.json"),
        format!(
            "{{\"schema\":1,\"gate\":\"B\",\"commit\":\"{commit}\",\"binary_sha256\":\"{sha}\"}}\n"
        ),
    )
    .unwrap();
}

#[test]
fn safety_ungated_apply_needs_stamp() {
    let Some(bin) = release_gate_b() else { return };
    let real_sha = sha256_of(&bin);
    let mut one_off: Vec<char> = real_sha.chars().collect();
    one_off[0] = if one_off[0] == '0' { '1' } else { '0' };
    let one_off: String = one_off.into_iter().collect();
    // (name, stamp to install)
    let legs: [(&str, Option<(String, String)>); 3] = [
        ("no stamp", None),
        (
            "a stamp for another commit",
            Some(("deadbeef".repeat(5), real_sha.clone())),
        ),
        (
            "a stamp whose binary_sha256 differs by one digit",
            Some((git_head(), one_off)),
        ),
    ];
    for (name, stamp) in legs {
        let f = Fixture::new();
        cold(&f);
        if let Some((commit, sha)) = &stamp {
            write_stamp(&f, commit, sha);
        }
        // No DC_TEST_ROOT: the run is unconfined, so it needs a stamp. HOME is the fixture's; the refusal comes
        // before any lock or audit file, so nothing is deleted even if the gate were open.
        let out = f
            .command_for(&bin)
            .env_remove("DC_TEST_ROOT")
            .args(["--apply", "--kinds", "caches"])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let (stdout, stderr) = text_of(&out);
        assert_eq!(out.status.code(), Some(64), "{name}: {stderr}");
        assert!(stdout.is_empty(), "{name}: {stdout}");
        assert!(
            stderr.contains("this binary has no valid gate stamp"),
            "{name}: {stderr}"
        );
        assert!(stderr.contains("Nothing was deleted"), "{name}: {stderr}");
        assert!(!caches_gone(&f), "{name}: a cache was deleted");
        let state = f.xdg("state").join("disk-cleanup");
        for created in ["run.lock", "audit.jsonl", "audit.reserve", "last_plan"] {
            assert!(
                !state.join(created).exists(),
                "{name}: {created} was created before the refusal"
            );
        }
    }
}

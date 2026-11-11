#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.2.4, spawned-binary acceptance criteria. Every run goes through `fixture_cmd()` (Story 1.1.6), so
//! HOME, the XDG directories and `DC_TEST_ROOT` are the fixture's and no real HOME, Docker or Steam is read.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use common::{tree_manifest, Fixture};
use disk_cleanup_core::ports::{classify, Class, Cmd};

fn with_cache(f: &Fixture) -> PathBuf {
    let dir = f.home().join(".npm/_cacache");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("blob"), vec![7u8; 1 << 20]).unwrap();
    dir
}

fn state_listing(f: &Fixture) -> BTreeSet<String> {
    std::fs::read_dir(f.xdg("state").join("disk-cleanup"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn text(o: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

/// Waits for `child`, killing it after `secs` so a hang fails the test and does not stall the suite.
fn wait_or_kill(mut child: std::process::Child, secs: u64) -> Option<Output> {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if child.try_wait().unwrap().is_some() {
            return Some(child.wait_with_output().unwrap());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

#[test]
fn bare_run_should_print_dry_run_and_leave_the_cache_and_the_whole_home_unchanged() {
    let f = Fixture::new();
    let dir = with_cache(&f);
    let before = tree_manifest(&f.home());
    let out = f.fixture_cmd().output().unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(stdout.contains("DRY RUN"), "{stdout}");
    assert!(
        stdout.contains(".npm/_cacache"),
        "the cache is listed: {stdout}"
    );
    assert!(dir.join("blob").is_file(), "the file still exists");
    assert_eq!(
        tree_manifest(&f.home()),
        before,
        "the fixture home is unchanged"
    );
}

#[test]
fn plan_mode_should_write_only_the_state_files_and_no_audit_log_or_run_lock() {
    let f = Fixture::new();
    with_cache(&f);
    let out = f.fixture_cmd().output().unwrap();
    let (_, stderr) = text(&out);
    assert!(
        stderr.contains("note: created 2 reserve files of 1 MiB"),
        "first run says so: {stderr}"
    );
    let want: BTreeSet<String> = [
        "audit.reserve",
        "last_plan",
        "last_plan.lock",
        "last_plan.reserve",
        "outcome.jsonl",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    assert_eq!(state_listing(&f), want);
    let again = f.fixture_cmd().output().unwrap();
    assert!(
        !text(&again).1.contains("created 2 reserve files"),
        "only the first run creates them"
    );
    let outcome =
        std::fs::read_to_string(f.xdg("state").join("disk-cleanup/outcome.jsonl")).unwrap();
    assert_eq!(outcome.lines().count(), 2, "one outcome line per run");
    assert!(!state_listing(&f).contains("audit.jsonl"));
    assert!(!state_listing(&f).contains("run.lock"));
}

#[test]
fn json_should_print_one_document_with_a_plan_id_and_nothing_else() {
    let f = Fixture::new();
    with_cache(&f);
    let out = f.fixture_cmd().arg("--json").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let d: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(d["schema_version"], 1);
    assert_eq!(d["mode"], "plan");
    assert!(common::extract_plan_id(&String::from_utf8_lossy(&out.stdout)).is_some());
    assert!(d["items"].as_array().unwrap().iter().any(|i| i["path"]
        .as_str()
        .is_some_and(|p| p.ends_with(".npm/_cacache"))));
}

#[test]
fn force_yes_and_y_should_exit_64_with_empty_stdout_and_the_tip_on_stderr() {
    let f = Fixture::new();
    let dir = with_cache(&f);
    for flag in ["--force", "-y", "--yes"] {
        let out = f.fixture_cmd().arg(flag).output().unwrap();
        let (stdout, stderr) = text(&out);
        assert_eq!(out.status.code(), Some(64), "{flag}");
        assert!(stdout.is_empty(), "{flag}: {stdout}");
        assert!(
            stderr.contains("Tip: there is no force or yes flag; use --kinds."),
            "{flag}: {stderr}"
        );
    }
    assert!(dir.join("blob").is_file());
}

#[test]
fn dry_run_flags_with_apply_should_exit_64_and_delete_nothing() {
    let f = Fixture::new();
    let dir = with_cache(&f);
    let combos: [&[&str]; 6] = [
        &["-n", "--apply"],
        &["--apply", "-n"],
        &["--dry-run", "--apply"],
        &["--apply", "--dry-run"],
        &["-n", "--apply", "--kinds", "caches"],
        &["--apply", "--dry-run", "--kinds", "caches"],
    ];
    for argv in combos {
        let out = f.fixture_cmd().args(argv).output().unwrap();
        let (stdout, stderr) = text(&out);
        assert_eq!(out.status.code(), Some(64), "{argv:?}");
        assert!(stdout.is_empty(), "{argv:?}: {stdout}");
        assert_eq!(
            stderr,
            "error: -n/--dry-run and --apply contradict each other; drop one. Dry run is the default, so plain 'disk-cleanup' is the dry run.\n"
        );
        assert!(dir.join("blob").is_file(), "{argv:?}");
    }
}

#[test]
fn apply_should_be_refused_with_the_gate_a_text_and_never_read_stdin_or_delete() {
    let f = Fixture::new();
    let dir = with_cache(&f);
    let child = f
        .fixture_cmd()
        .arg("--apply")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // stdin stays open and is never written: a prompt would hang here and the watchdog fails the test.
    let out = wait_or_kill(child, 20).expect("--apply must not wait for input");
    let (stdout, stderr) = text(&out);
    if cfg!(feature = "gate-b") {
        // A build that reached Gate B registers `caches`: the bare apply runs and needs no input.
        assert_eq!(out.status.code(), Some(0), "{stderr}");
        assert!(stdout.contains("APPLY FINISHED - exit 0."), "{stdout}");
        return;
    }
    assert_eq!(out.status.code(), Some(64));
    assert!(stdout.is_empty());
    assert!(
        stderr.contains("apply is not enabled in this build"),
        "{stderr}"
    );
    assert!(dir.join("blob").is_file());
    assert!(
        !state_listing(&f).contains("audit.jsonl"),
        "a refused apply writes nothing: {:?}",
        state_listing(&f)
    );
}

#[test]
fn a_named_kind_this_build_does_not_ship_should_exit_64_with_the_catalogue_text() {
    let f = Fixture::new();
    let out = f
        .fixture_cmd()
        .args(["--kinds", "build-dirs"])
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(64));
    assert!(stdout.is_empty());
    assert!(
        stderr.contains("'build-dirs' is not available in this build (see 'disk-cleanup kinds')"),
        "{stderr}"
    );
    let out = f.fixture_cmd().args(["--kinds", "dockr"]).output().unwrap();
    assert_eq!(out.status.code(), Some(64));
    assert!(text(&out)
        .1
        .contains("unknown kind 'dockr'. Did you mean 'docker'?"));
}

/// Shims for the tools a plan run may ask about, each logging its argv.
fn shims(f: &Fixture) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let bin = f.root().join("shims");
    std::fs::create_dir_all(&bin).unwrap();
    let log = f.root().join("shim.log");
    for tool in ["docker", "go", "pip", "pip3", "git", "brew", "btrfs"] {
        let answer = match tool {
            "go" => format!("echo {}/.cache/go-build", f.home().display()),
            "pip" | "pip3" => format!("echo {}/.cache/pip", f.home().display()),
            _ => "exit 0".to_string(),
        };
        let body = format!(
            "#!/bin/sh\necho \"{tool} $*\" >> '{}'\n{answer}\n",
            log.display()
        );
        let p = bin.join(tool);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (bin, log)
}

#[test]
fn plan_mode_should_run_only_read_only_allowlisted_commands() {
    let f = Fixture::new();
    with_cache(&f);
    let (bin, log) = shims(&f);
    let mut cmd = f.fixture_cmd();
    cmd.env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out).1);
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        logged.lines().any(|l| l.starts_with("go ")),
        "go env GOCACHE was asked: {logged:?}"
    );
    for line in logged.lines() {
        let mut w = line.split_whitespace();
        let prog = w.next().unwrap();
        let cmd = Cmd::new(prog, w, false);
        assert_eq!(
            classify(&cmd),
            Some(Class::ReadOnly),
            "plan mode ran {line:?}, which is not a ReadOnly row"
        );
    }
    assert!(!state_listing(&f).contains("audit.jsonl"));
}

#[test]
fn a_closed_stdout_should_end_a_plan_run_quietly_with_exit_0() {
    let f = Fixture::new();
    with_cache(&f);
    let mut child = f
        .fixture_cmd()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let out = wait_or_kill(child, 30).expect("the run ends");
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(
        !stderr.contains("panicked") && !stderr.contains("Broken pipe"),
        "{stderr}"
    );
}

// ---------------------------------------------------------------- status

const GIB: u64 = 1 << 30;

/// A `/sys/fs/btrfs/<uuid>` fixture: 100 GiB of devices with `unallocated` left over.
fn sysfs(root: &Path, unallocated: u64) -> PathBuf {
    let dir = root.join("sysfs-btrfs-uuid");
    let device = 100 * GIB;
    let allocated = device - unallocated;
    let (data, meta, sys) = (allocated - 4 * GIB - 8 * (1 << 20), 4 * GIB, 8 * (1 << 20));
    let put = |rel: &str, v: u64| {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, format!("{v}\n")).unwrap();
    };
    for (name, disk_total, used) in [
        ("data", data, data / 2),
        ("metadata", meta, GIB / 2),
        ("system", sys, 1 << 20),
    ] {
        put(&format!("allocation/{name}/total_bytes"), disk_total);
        put(&format!("allocation/{name}/bytes_used"), used);
        put(&format!("allocation/{name}/disk_total"), disk_total);
        put(&format!("allocation/{name}/disk_used"), used);
    }
    put("allocation/global_rsv_size", 512 << 20);
    put("allocation/global_rsv_reserved", 0);
    put("devices/sda1/size", device / 512);
    dir
}

fn status_json(f: &Fixture, fs: &Path) -> (Option<i32>, serde_json::Value) {
    let out = f
        .fixture_cmd()
        .args(["status", "--json", "--fs"])
        .arg(fs)
        .output()
        .unwrap();
    let doc = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
    (out.status.code(), doc)
}

#[test]
fn status_fs_json_should_carry_the_old_metascope_keys_with_their_values_and_exit_0_1_2() {
    let f = Fixture::new();
    for (unalloc, code, verdict) in [
        (98 * GIB / 10, 1, "warn"),
        (2 * GIB, 2, "crit"),
        (50 * GIB, 0, "ok"),
    ] {
        let dir = sysfs(f.root(), unalloc);
        let (exit, d) = status_json(&f, &dir);
        assert_eq!(exit, Some(code), "{verdict}: {d}");
        // The six keys of `metascope status --json` at 3d36581, with their old values.
        assert_eq!(d["fs"], dir.display().to_string());
        assert_eq!(d["unallocated_bytes"], unalloc);
        assert_eq!(d["metadata_headroom_bytes"], 4 * GIB - GIB / 2);
        assert_eq!(d["verdict"], verdict);
        assert_eq!(d["allocation"]["device_bytes"], 100 * GIB);
        assert_eq!(d["allocation"]["metadata"]["total_bytes"], 4 * GIB);
        assert_eq!(d["allocation"]["metadata"]["bytes_used"], GIB / 2);
        assert_eq!(d["allocation"]["global_rsv_size"], 512 << 20);
        for k in ["data", "system"] {
            assert!(d["allocation"][k]["disk_total"].is_u64(), "{k}");
        }
        let findings = d["findings"].as_array().unwrap();
        assert_eq!(findings.is_empty(), verdict == "ok", "{d}");
        assert!(findings
            .iter()
            .all(|x| x["severity"].is_string() && x["message"].is_string()));
        // The superset: the keys the new documents add.
        assert_eq!(d["schema_version"], 1);
        assert_eq!(d["mode"], "status");
        assert_eq!(d["exit_code"], code);
        assert!(d["errors"].as_array().unwrap().is_empty());
    }
    // PATH defaults to `/`.
    assert_eq!(status_json(&f, &sysfs(f.root(), 50 * GIB)).1["path"], "/");
}

#[test]
fn status_should_exit_74_when_the_fs_directory_is_unreadable_and_write_only_reserves_and_the_outcome_line(
) {
    let f = Fixture::new();
    let out = f
        .fixture_cmd()
        .args(["status", "--fs", "/definitely/not/a/btrfs/dir"])
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(74));
    assert!(stdout.is_empty());
    assert!(
        stderr.contains("error: cannot read filesystem state for /definitely/not/a/btrfs/dir:"),
        "{stderr}"
    );
    let (code, d) = status_json(&f, Path::new("/definitely/not/a/btrfs/dir"));
    assert_eq!(code, Some(74));
    assert_eq!(d["verdict"], "unknown");
    assert_eq!(d["errors"][0]["code"], "state_unreadable");
    // A good read: only the reserves and the one outcome line appear.
    let g = Fixture::new();
    let _ = status_json(&g, &sysfs(g.root(), 50 * GIB));
    let want: BTreeSet<String> = ["audit.reserve", "last_plan.reserve", "outcome.jsonl"]
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(state_listing(&g), want);
    let line = std::fs::read_to_string(g.xdg("state").join("disk-cleanup/outcome.jsonl")).unwrap();
    assert_eq!(line.lines().count(), 1);
    assert!(line.contains("\"mode\":\"status\""), "{line}");
}

#[test]
fn status_text_should_use_the_s4_block_and_a_missing_path_should_exit_64() {
    let f = Fixture::new();
    let out = f
        .fixture_cmd()
        .args(["status", "--fs"])
        .arg(sysfs(f.root(), 98 * GIB / 10))
        .output()
        .unwrap();
    let (stdout, _) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.starts_with("Filesystem /  (btrfs)    verdict: WARN"),
        "{stdout}"
    );
    assert!(stdout.contains("unallocated    9.8 GiB"), "{stdout}");
    assert!(stdout.contains("Hint: 'disk-cleanup' (dry run) lists what could be freed."));
    let out = f
        .fixture_cmd()
        .args(["status", "/no/such/path"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(64));
    assert!(out.stdout.is_empty());
}

#[test]
fn kinds_should_list_every_selector_with_availability_and_exit_0() {
    let f = Fixture::new();
    let out = f.fixture_cmd().arg("kinds").output().unwrap();
    let (stdout, _) = text(&out);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout.lines().next().unwrap().starts_with("ID"));
    let row = |id: &str| {
        stdout
            .lines()
            .find(|l| l.split_whitespace().next() == Some(id))
            .unwrap_or_else(|| panic!("{id} missing: {stdout}"))
            .to_string()
    };
    // Caches are listed in every build and can delete only in a build that reached Gate B (Story 2.3.4).
    let caches = row("caches");
    if cfg!(feature = "gate-b") {
        assert!(caches.contains(" yes "), "{caches}");
    } else {
        assert!(caches.contains("list only"), "{caches}");
    }
    for unbuilt in ["worktrees", "build-dirs", "docker", "steam", "btrfs"] {
        if cfg!(feature = "gate-c") && unbuilt == "worktrees" {
            continue;
        }
        assert!(
            row(unbuilt).contains("not available in this build"),
            "{unbuilt}"
        );
    }
    let json = f.fixture_cmd().args(["kinds", "--json"]).output().unwrap();
    let d: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    // The ten selectors plus the test-only `fixture-destructive`, which a `test-support` build lists under
    // `DC_TEST_ROOT` (Story 2.3.4).
    assert_eq!(d["kinds"].as_array().unwrap().len(), 11);
    assert!(row("fixture-destructive").contains("destructive"));
}

#[test]
fn a_malformed_plan_id_should_exit_64_with_empty_stdout() {
    let f = Fixture::new();
    for id in ["91be04", "zzzzzzzz", "0x7f3a91c", ""] {
        let out = f
            .fixture_cmd()
            .args(["--apply", "--kinds", "build-dirs", "--plan-id", id])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(64), "{id:?}");
        assert!(out.stdout.is_empty(), "{id:?}");
        assert!(text(&out).1.contains("a plan-id is 8 hex digits"));
    }
}

#[test]
fn help_and_version_should_exit_0_on_stdout_and_a_usage_error_should_exit_64() {
    let f = Fixture::new();
    let out = f.fixture_cmd().arg("--help").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(text(&out)
        .0
        .starts_with("Free disk space. Dry run by default: nothing is deleted without --apply."));
    let out = f.fixture_cmd().arg("--version").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(text(&out).0.starts_with("disk-cleanup "));
    let out = f.fixture_cmd().arg("frobnicate").output().unwrap();
    assert_eq!(out.status.code(), Some(64));
    assert!(out.stdout.is_empty());
}

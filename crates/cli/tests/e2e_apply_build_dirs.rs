#![cfg(feature = "gate-c")]
#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 3.2.4: Gate C, `--apply --kinds build-dirs` end to end through the spawned binary under
//! `fixture_cmd()`. Run with `GATE=C cargo test --workspace --features disk-cleanup/gate-c`; a build without
//! the feature has no `build-dirs` kind, so this file is compiled only with it.
//!
//! Every directory lives in a tempdir fixture HOME; nothing here touches a real repository or build dir.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;

use common::{aged_dir, extract_plan_id, next_commands, run_line, text_of, tree_manifest, Fixture};

fn plain(f: &Fixture, args: &[&str]) -> Output {
    f.fixture_cmd()
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

/// `~/code/<rel>/{package.json, package-lock.json, node_modules/f}`, the build dir aged `days`.
fn project(f: &Fixture, rel: &str, lock: &str, days: u64) -> PathBuf {
    project_sized(f, rel, lock, days, 20_000)
}

fn project_sized(f: &Fixture, rel: &str, lock: &str, days: u64, bytes: usize) -> PathBuf {
    let dir = f.home().join("code").join(rel);
    let nm = dir.join("node_modules");
    aged_dir(&nm, bytes, days);
    std::fs::write(dir.join("package.json"), "{}").unwrap();
    std::fs::write(dir.join("package-lock.json"), lock).unwrap();
    nm
}

fn two_stale(f: &Fixture) -> [PathBuf; 2] {
    [
        project(f, "alpha", "lock-alpha", 90),
        project(f, "beta", "lock-beta", 90),
    ]
}

fn dry_id(f: &Fixture, extra: &[&str]) -> String {
    let mut args = vec!["--kinds", "build-dirs", "--json"];
    args.extend_from_slice(extra);
    let out = plain(f, &args);
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    extract_plan_id(&text_of(&out).0).expect("plan id")
}

fn apply(f: &Fixture, id: &str, extra: &[&str]) -> Output {
    let mut args = vec!["--apply", "--kinds", "build-dirs", "--plan-id", id];
    args.extend_from_slice(extra);
    plain(f, &args)
}

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
    let entry = doc["entries"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|e| e["plan_id"] == id)
        .unwrap_or_else(|| panic!("no saved entry {id}"));
    entry["saved_at"] = serde_json::json!(now - age_secs);
    std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
}

fn refused(out: &Output, needle: &str) {
    let (stdout, stderr) = text_of(out);
    assert_eq!(out.status.code(), Some(64), "{stderr}");
    assert!(stdout.is_empty(), "stdout must be empty: {stdout}");
    assert!(
        stderr.to_lowercase().contains(&needle.to_lowercase()),
        "stderr lacks {needle:?}: {stderr}"
    );
}

fn run_start_ids(f: &Fixture) -> Vec<String> {
    std::fs::read_to_string(f.xdg("state").join("disk-cleanup/audit.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["type"] == "run_start")
        .filter_map(|v| v["plan_id"].as_str().map(str::to_string))
        .collect()
}

// ------------------------------------------------------------------ registered at Gate C

#[test]
fn kinds_should_list_build_dirs_as_a_destructive_kind_that_can_delete_When_the_gate_c_build_runs() {
    let f = Fixture::new();
    let out = plain(&f, &["kinds", "--json"]);
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let row = doc["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == "build-dirs")
        .expect("build-dirs is listed");
    assert_eq!(row["available"], "yes");
    assert_eq!(row["tier"], "destructive");
}

// ------------------------------------------------------------------ the destructive-kind gate

#[test]
fn apply_should_exit_64_and_delete_nothing_When_the_plan_id_is_missing_or_altered() {
    let f = Fixture::new();
    let nms = two_stale(&f);
    let before = tree_manifest(&f.home());

    refused(
        &plain(&f, &["--apply", "--kinds", "build-dirs"]),
        "need --plan-id",
    );
    assert_eq!(tree_manifest(&f.home()), before);

    let id = dry_id(&f, &[]);
    let mut digits: Vec<char> = id.chars().collect();
    digits[0] = if digits[0] == '0' { '1' } else { '0' };
    let altered: String = digits.into_iter().collect();
    refused(&apply(&f, &altered, &[]), "no dry run with plan-id");
    assert_eq!(tree_manifest(&f.home()), before);
    assert!(nms.iter().all(|n| n.is_dir()));
}

#[test]
fn apply_should_exit_64_When_the_saved_plan_is_25_hours_old() {
    let f = Fixture::new();
    two_stale(&f);
    let id = dry_id(&f, &[]);
    age_saved_plan(&f, &id, 25 * 3600);
    let before = tree_manifest(&f.home());
    refused(&apply(&f, &id, &[]), "24");
    assert_eq!(tree_manifest(&f.home()), before);
}

#[test]
fn apply_should_exit_64_When_a_build_dir_appeared_after_the_dry_run() {
    let f = Fixture::new();
    two_stale(&f);
    let id = dry_id(&f, &[]);
    project(&f, "gamma", "lock-gamma", 90);
    let before = tree_manifest(&f.home());
    refused(
        &apply(&f, &id, &[]),
        "does not match the plan just computed",
    );
    assert_eq!(tree_manifest(&f.home()), before);
}

#[test]
fn apply_should_remove_both_dirs_and_log_the_plan_id_When_the_id_comes_from_a_dry_run_of_the_same_selection(
) {
    let f = Fixture::new();
    let nms = two_stale(&f);
    let id = dry_id(&f, &[]);

    let out = apply(&f, &id, &[]);

    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    assert!(
        nms.iter().all(|n| !n.exists()),
        "a build dir is still there"
    );
    assert!(
        f.home().join("code/alpha/package.json").is_file(),
        "only the build dir goes"
    );
    assert_eq!(run_start_ids(&f), vec![id]);

    // `--apply` with the default kinds still needs no id and never reaches build dirs.
    let f = Fixture::new();
    let nms = two_stale(&f);
    let out = plain(&f, &["--apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    assert!(
        nms.iter().all(|n| n.is_dir()),
        "the default set touched a build dir"
    );
}

// ------------------------------------------------------------------ the Gate B flow cases on the real kind

/// Two copies of one lockfile (a duplicate set), and a lonely, a mid-aged, a tiny and a young dir.
fn mixed(f: &Fixture) -> [&'static str; 6] {
    project(f, "dup-old", "same-lock", 90);
    project(f, "dup-new", "same-lock", 70);
    project(f, "lonely", "lock-lonely", 80);
    project(f, "mid", "lock-mid", 30);
    project(f, "young", "lock-young", 0);
    project_sized(f, "tiny", "lock-tiny", 85, 1);
    [
        "code/dup-old/node_modules",
        "code/dup-new/node_modules",
        "code/lonely/node_modules",
        "code/mid/node_modules",
        "code/young/node_modules",
        "code/tiny/node_modules",
    ]
}

struct Case {
    flags: &'static [&'static str],
    /// Dirs (relative to HOME) the pasted line removes; every other build dir stays.
    removed: &'static [&'static str],
    /// The flag (with its value) to drop from the pasted line; the line then exits 64 with `changed`.
    drop: Option<&'static str>,
}

fn run_case(case: &Case) {
    let f = Fixture::new();
    let all = mixed(&f);
    let mut dry_args = vec!["--kinds", "build-dirs"];
    dry_args.extend_from_slice(case.flags);
    let dry = plain(&f, &dry_args);
    assert_eq!(
        dry.status.code(),
        Some(0),
        "{dry_args:?}: {}",
        text_of(&dry).1
    );
    let cmds = next_commands(&text_of(&dry).0);
    let with_id = cmds
        .iter()
        .find(|c| c.contains("--plan-id"))
        .unwrap_or_else(|| {
            panic!(
                "no Next line carries --plan-id: {cmds:?}\n{}",
                text_of(&dry).0
            )
        })
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
        refused(&out, "changed");
        for s in all {
            assert!(
                f.home().join(s).exists(),
                "{s} was deleted by the refused line"
            );
        }
    }
    let out = run_line(&f, &with_id);
    assert_eq!(out.status.code(), Some(0), "{with_id}: {}", text_of(&out).1);
    for s in all {
        assert_eq!(
            f.home().join(s).exists(),
            !case.removed.contains(&s),
            "{s} after `{with_id}`"
        );
    }
}

const DUP_OLD: &str = "code/dup-old/node_modules";
const DUP_NEW: &str = "code/dup-new/node_modules";
const LONELY: &str = "code/lonely/node_modules";
const MID: &str = "code/mid/node_modules";
const TINY: &str = "code/tiny/node_modules";

#[test]
fn a_pasted_next_line_should_apply_the_reviewed_rows_and_refuse_the_line_with_one_flag_removed() {
    // Default keep-newest is 1: the newer copy of the duplicate set survives. 60d leaves `mid` (30d) alone,
    // and without the flag the 14d default would offer it.
    run_case(&Case {
        flags: &["--min-age", "60d"],
        removed: &[DUP_OLD, LONELY, TINY],
        drop: Some("--min-age"),
    });
    // `--keep-newest 0` offers the newest copy too; without the flag the default 1 keeps it.
    run_case(&Case {
        flags: &["--keep-newest", "0"],
        removed: &[DUP_OLD, DUP_NEW, LONELY, MID, TINY],
        drop: Some("--keep-newest"),
    });
    // 16KiB drops the tiny dir; 1KiB drops nothing, so the line without it is the same plan.
    run_case(&Case {
        flags: &["--min-size", "16KiB"],
        removed: &[DUP_OLD, LONELY, MID],
        drop: Some("--min-size"),
    });
    run_case(&Case {
        flags: &["--min-size", "1KiB"],
        removed: &[DUP_OLD, LONELY, MID, TINY],
        drop: None,
    });
}

#[test]
fn a_pasted_next_line_should_stay_inside_the_root_and_carry_the_resolved_kinds_When_root_and_except_are_given(
) {
    let f = Fixture::new();
    let all = mixed(&f);
    let root = f.home().join("code/lonely");
    let dry = plain(
        &f,
        &[
            "--kinds",
            "caches,build-dirs",
            "--except",
            "caches",
            "--root",
            root.to_str().unwrap(),
        ],
    );
    assert_eq!(dry.status.code(), Some(0), "{}", text_of(&dry).1);
    let cmds = next_commands(&text_of(&dry).0);
    let with_id = cmds.iter().find(|c| c.contains("--plan-id")).unwrap();
    assert!(with_id.contains("--kinds build-dirs"), "{with_id}");
    assert!(with_id.contains("--root"), "{with_id}");
    let out = run_line(&f, with_id);
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    for s in all {
        assert_eq!(f.home().join(s).exists(), s != LONELY, "{s}");
    }
}

#[test]
fn keep_newest_should_exit_64_When_build_dirs_is_not_in_kinds_and_the_zero_warning_should_print() {
    let f = Fixture::new();
    mixed(&f);
    let out = plain(&f, &["--kinds", "caches", "--keep-newest", "2"]);
    refused(&out, "--keep-newest only applies to build-dirs");

    let out = plain(&f, &["--kinds", "build-dirs", "--keep-newest", "0"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        text_of(&out).0.contains(
            "WARNING: --keep-newest 0 also offers the newest copy of every duplicate set"
        ),
        "{}",
        text_of(&out).0
    );
    let out = plain(&f, &["--kinds", "build-dirs", "--min-age", "5m"]);
    assert!(
        text_of(&out)
            .0
            .contains("build-dirs: --min-age raised to the 60m floor"),
        "{}",
        text_of(&out).0
    );
}

#[test]
fn the_floor_should_keep_a_young_dir_When_min_age_is_zero() {
    let f = Fixture::new();
    let all = mixed(&f);
    let id = dry_id(&f, &["--min-age", "0"]);
    let out = apply(&f, &id, &["--min-age", "0"]);
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    assert!(
        f.home().join(all[4]).exists(),
        "a dir younger than 60 minutes was removed"
    );
}

// ------------------------------------------------------------------ recovery after a partly failed apply

fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|o| o.stdout == b"0\n")
}

#[test]
fn a_partly_failed_apply_should_spend_the_id_and_a_new_dry_run_should_finish_the_job() {
    if is_root() {
        eprintln!("SKIP: root ignores directory permissions, the failure cannot be provoked");
        return;
    }
    let f = Fixture::new();
    let nms: Vec<PathBuf> = (0..5)
        .map(|i| project(&f, &format!("p{i}"), &format!("lock-{i}"), 90))
        .collect();
    // Dir 3's parent is read-only: the tree empties, then the final rmdir in the parent fails with EACCES.
    let stuck_parent = nms[2].parent().unwrap().to_path_buf();
    std::fs::set_permissions(&stuck_parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    let a = dry_id(&f, &[]);

    let out = apply(&f, &a, &["--jobs", "1"]);
    let (stdout, stderr) = text_of(&out);
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stdout.contains(&format!(
            "plan-id {a} is spent (4 items removed). Dry run again, then use the new plan-id: disk-cleanup --kinds build-dirs"
        )),
        "{stdout}"
    );
    let left: Vec<&PathBuf> = nms.iter().filter(|n| n.exists()).collect();
    assert_eq!(left, vec![&nms[2]]);

    // The same apply with the spent id: `changed`, vanished list, nothing more deleted.
    let again = apply(&f, &a, &["--jobs", "1"]);
    refused(&again, "does not match the plan just computed");
    let err = text_of(&again).1;
    assert!(err.contains("vanished"), "{err}");
    assert!(
        err.contains("expected"),
        "the partial-apply sentence is missing: {err}"
    );

    // Once the cause is gone a new dry run has nothing left to offer: the failed dir was emptied by the
    // attempt, so its mtime is fresh and the 60 minute floor keeps it (plan defect, phase5-findings.md).
    std::fs::set_permissions(&stuck_parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let b = dry_id(&f, &[]);
    assert_ne!(a, b);
    let out = apply(&f, &b, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    assert!(
        text_of(&out).0.contains("APPLY FINISHED - exit 0."),
        "{}",
        text_of(&out).0
    );
    assert!(nms[2].is_dir() && std::fs::read_dir(&nms[2]).unwrap().next().is_none());
}

// ------------------------------------------------------------------ plan-id across users

#[test]
fn apply_should_exit_64_with_the_uid_sentence_and_delete_nothing_When_a_different_user_applies() {
    if !common::unshare_works() {
        assert!(
            std::env::var_os("DC_REQUIRE_UNSHARE").is_none(),
            "DC_REQUIRE_UNSHARE is set"
        );
        eprintln!("SKIP: unshare -rm does not work here (DC_REQUIRE_UNSHARE=1 makes it a failure)");
        return;
    }
    let f = Fixture::new();
    let nms = two_stale(&f);
    let id = dry_id(&f, &[]);
    let before = tree_manifest(&f.home());

    // Same state directory, but euid 0 inside a user namespace with its own HOME (`/root` in the plan's words):
    // the root's HOME holds other items, so the ids differ and the refusal names the uid.
    let root_home = f.root().join("root-home");
    std::fs::create_dir_all(root_home.join("code")).unwrap();
    let out = f
        .command_for("unshare")
        .env("HOME", &root_home)
        .args(["-rm", env!("CARGO_BIN_EXE_disk-cleanup")])
        .args(["--apply", "--kinds", "build-dirs", "--plan-id", &id])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();

    let (stdout, stderr) = text_of(&out);
    assert_eq!(out.status.code(), Some(64), "{stdout}\n{stderr}");
    assert!(stdout.is_empty());
    assert!(stderr.contains("That dry run was made as uid"), "{stderr}");
    assert_eq!(tree_manifest(&f.home()), before);
    assert!(nms.iter().all(|n| n.is_dir()));
}

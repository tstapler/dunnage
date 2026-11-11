#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.3.3 (Gate A): the read-only report end to end on a fixture HOME, the symlinked-root `Notes:` line,
//! and the refusal of `--apply` in this build. Every run goes through the harness (`env_clear`, `DC_TEST_ROOT`
//! fixture, a PATH without brew, docker or steam), and nothing here passes `--apply` to a build that would honour it.

mod common;

use common::{tree_manifest, Fixture};
use serde_json::Value;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Output};

/// PATH with the read-only system tools and nothing that could mutate (no brew, steam; `docker` is a shim whose daemon is down).
const SAFE_PATH: &str = "/usr/bin:/bin";

/// A `go` that answers `go env GOCACHE` with the fixture's cache directory and does nothing else.
fn stub_go(f: &Fixture, xdg_cache: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let bin = f.root().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    common::docker_down_shim(&bin);
    let go = bin.join("go");
    std::fs::write(
        &go,
        format!("#!/bin/sh\necho '{}/go-build'\n", xdg_cache.display()),
    )
    .unwrap();
    std::fs::set_permissions(&go, std::fs::Permissions::from_mode(0o755)).unwrap();
    format!("{}:{SAFE_PATH}", bin.display())
}

fn plan_cmd(f: &Fixture, xdg_cache: &Path) -> Command {
    let path = stub_go(f, xdg_cache);
    let mut c = f.fixture_cmd();
    // The cwd is a scan-root candidate for the cleaner, so it must be the fixture and not the crate directory.
    c.env("PATH", path)
        .env("XDG_CACHE_HOME", xdg_cache)
        .current_dir(f.home());
    c
}

fn json_of(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "{:?} {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}): {:?}", out.stdout))
}

fn items<'a>(doc: &'a Value, family: &str) -> Vec<&'a Value> {
    doc["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|i| i["family"] == family)
        .collect()
}

fn flat(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fill(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("blob"), vec![7u8; 64 * 1024]).unwrap();
}

#[test]
fn report_should_exit_0_print_one_json_document_with_npm_and_go_caches_and_leave_home_unchanged() {
    let f = Fixture::new();
    fill(&f.home().join(".npm/_cacache"));
    fill(&f.home().join(".cache/go-build"));
    let before = tree_manifest(&f.home());
    let out = plan_cmd(&f, &f.home().join(".cache"))
        .arg("--json")
        .output()
        .unwrap();
    let doc = json_of(&out);
    assert_eq!(doc["mode"], "plan");
    assert_eq!(doc["exit_code"], 0);
    let caches = items(&doc, "caches");
    let has = |tail: &str| {
        caches
            .iter()
            .any(|i| i["path"].as_str().is_some_and(|p| p.ends_with(tail)))
    };
    assert!(has(".npm/_cacache"), "no npm cache item: {caches:?}");
    assert!(has("go-build"), "no go cache item: {caches:?}");
    let npm = caches
        .iter()
        .find(|i| i["path"].as_str().unwrap().ends_with(".npm/_cacache"))
        .unwrap();
    assert_eq!(npm["status"], "would_free");
    assert!(npm["est_bytes"].as_u64().unwrap() >= 64 * 1024);
    assert_eq!(tree_manifest(&f.home()), before, "a plan run changed HOME");
}

#[test]
fn report_should_carry_discover_and_plan_timings_in_the_plan_document_When_json() {
    let f = Fixture::new();
    fill(&f.home().join(".npm/_cacache"));
    let out = plan_cmd(&f, &f.home().join(".cache"))
        .arg("--json")
        .output()
        .unwrap();
    let doc = json_of(&out);
    for k in ["discover_ms", "plan_ms"] {
        assert!(doc["timings"][k].is_u64(), "timings.{k} missing: {doc}");
    }
}

#[test]
fn report_should_print_a_dry_run_text_report_by_default_and_change_nothing() {
    let f = Fixture::new();
    fill(&f.home().join(".npm/_cacache"));
    let before = tree_manifest(&f.home());
    let out = plan_cmd(&f, &f.home().join(".cache")).output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("DRY RUN - nothing will be deleted."),
        "{text}"
    );
    assert!(text.contains("DRY RUN - nothing was deleted."), "{text}");
    assert_eq!(tree_manifest(&f.home()), before);
}

#[test]
fn report_should_list_a_symlinked_cache_root_in_notes_and_leave_its_target_untouched() {
    let f = Fixture::new();
    let real = f.root().join("data/cache");
    fill(&real.join("go-build"));
    symlink(&real, f.home().join(".cache")).unwrap();
    let before = tree_manifest(&real);
    let cache = f.home().join(".cache");

    let out = plan_cmd(&f, &cache).arg("--json").output().unwrap();
    let doc = json_of(&out);
    let want = format!(
        "symlinked roots: ~/.cache -> {} (1 cache kind is skipped: the tool never follows a symlinked parent; point --root at the real directory or move the data)",
        real.display()
    );
    let notes: Vec<&str> = doc["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap())
        .collect();
    assert!(notes.contains(&want.as_str()), "{want}\nnotes: {notes:?}");
    let go: Vec<_> = items(&doc, "caches")
        .into_iter()
        .filter(|i| i["path"].as_str().unwrap().ends_with(".cache/go-build"))
        .collect();
    assert_eq!(go.len(), 1, "{doc}");
    assert_eq!(go[0]["status"], "skipped");
    assert_eq!(go[0]["reason"], "symlink");
    assert_eq!(go[0]["reason_text"], "out of scope: symlink");

    let text_out = plan_cmd(&f, &cache).output().unwrap();
    assert!(text_out.status.success());
    let text = String::from_utf8_lossy(&text_out.stdout).into_owned();
    let notes_at = text.find("Notes:").expect("a Notes: block");
    assert!(
        flat(&text[notes_at..]).contains(&want),
        "Notes block lacks the line:\n{text}"
    );
    assert!(
        text.lines().any(|l| l.starts_with("SKIP")
            && l.contains("~/.cache/go-build")
            && l.contains("symlink")),
        "{text}"
    );
    assert_eq!(tree_manifest(&real), before, "the symlink target changed");
}

#[test]
fn report_should_list_a_symlinked_cargo_registry_in_notes() {
    let f = Fixture::new();
    let real = f.root().join("data/registry");
    fill(&real.join("cache"));
    fill(&real.join("src"));
    std::fs::create_dir_all(f.home().join(".cargo")).unwrap();
    symlink(&real, f.home().join(".cargo/registry")).unwrap();
    let before = tree_manifest(&real);
    let out = plan_cmd(&f, &f.home().join(".cache"))
        .arg("--json")
        .output()
        .unwrap();
    let doc = json_of(&out);
    let prefix = format!("symlinked roots: ~/.cargo/registry -> {} (", real.display());
    let note = doc["notes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .find(|n| n.starts_with(&prefix))
        .unwrap_or_else(|| panic!("no registry note in {:?}", doc["notes"]));
    assert!(note.contains("cache kind"), "{note}");
    assert!(items(&doc, "caches")
        .iter()
        .any(|i| i["status"] == "skipped" && i["reason"] == "symlink"));
    assert_eq!(tree_manifest(&real), before);
}

#[test]
fn report_should_print_no_symlink_note_when_no_root_is_a_symlink() {
    let f = Fixture::new();
    fill(&f.home().join(".cache/go-build"));
    let out = plan_cmd(&f, &f.home().join(".cache"))
        .arg("--json")
        .output()
        .unwrap();
    let doc = json_of(&out);
    assert!(
        !doc["notes"].to_string().contains("symlinked roots"),
        "{}",
        doc["notes"]
    );
}

#[test]
fn apply_should_exit_64_with_not_enabled_in_this_build_and_touch_nothing() {
    if cfg!(feature = "gate-b") {
        // A Gate B build can apply caches; its end-to-end cases are Story 2.3.3's `e2e_apply_caches.rs`.
        return;
    }
    let f = Fixture::new();
    fill(&f.home().join(".npm/_cacache"));
    let before = tree_manifest(&f.home());
    for extra in [&["--apply"][..], &["--apply", "--plan-id", "deadbeef"]] {
        let out = plan_cmd(&f, &f.home().join(".cache"))
            .args(extra)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(64), "{extra:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("apply is not enabled in this build"), "{err}");
        assert!(out.stdout.is_empty(), "{:?}", out.stdout);
    }
    assert_eq!(tree_manifest(&f.home()), before);
}

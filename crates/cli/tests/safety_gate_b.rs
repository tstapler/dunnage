#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! The Gate B safety guarantees of `ci/safety/manifest.txt` (Story 2.3.3), each a test named `safety_*` so
//! `make safety` runs them on both profiles and `scripts/check-safety-manifest.sh` can prove the list complete.
//! Every run is the spawned binary under `fixture_cmd()` (or a pure call), on a fixture HOME only.
//!
//! The guarantees that need a deleting build (`caches` applied) are compiled with the `gate-b` feature;
//! `make safety` passes it.

mod common;

use std::ffi::OsString;
use std::path::PathBuf;

use common::{aged_dir, text_of, tree_hash, tree_manifest, Fixture};
use disk_cleanup_core::ports::{Cmd, CmdOutput, CommandRunner, DryRunRunner};
use disk_cleanup_core::testing::FakeCommandRunner;
use disk_cleanup_core::{AllowedTarget, DeleteError, DeletePolicy, Identity, NamePattern, RootDev};

fn populate(f: &Fixture) {
    let home = f.home();
    for (rel, bytes) in [
        (".npm/_cacache/a", 30_000),
        (".cargo/registry/src/crate-1/lib.rs", 10_000),
        ("Documents/thesis.txt", 500),
        (".ssh/id_ed25519", 100),
    ] {
        let p = home.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, vec![9u8; bytes]).unwrap();
    }
    aged_dir(&home.join("code/proj/stale-a"), 9_000, 90);
}

#[test]
fn safety_bare_run_deletes_nothing() {
    let f = Fixture::new();
    populate(&f);
    let before = (tree_manifest(f.root()), tree_hash(&f.home()));
    // The state directory is outside the hashed home, so the plan's `last_plan` does not count as a change.
    for args in [
        vec![],
        vec!["--json"],
        vec!["--kinds", "caches,fixture-destructive"],
    ] {
        let out = f
            .fixture_cmd()
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", text_of(&out).1);
        assert!(!out.stdout.is_empty(), "{args:?} printed nothing");
    }
    let after_manifest = tree_manifest(f.root());
    let before_manifest = &before.0;
    // Only the state directory (xdg) may differ; everything else under the fixture root is byte-identical.
    let strip = |m: &std::collections::BTreeMap<PathBuf, String>| {
        m.iter()
            .filter(|(p, _)| !p.starts_with("xdg"))
            .map(|(p, v)| (p.clone(), v.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(strip(&after_manifest), strip(before_manifest));
    assert_eq!(tree_hash(&f.home()), before.1);
    assert!(
        f.xdg("state").join("disk-cleanup/last_plan").is_file(),
        "vacuous: the dry run saved nothing"
    );
}

#[test]
fn safety_force_and_yes_flags_rejected() {
    let f = Fixture::new();
    populate(&f);
    let before = tree_manifest(f.root());
    for args in [
        vec!["--force"],
        vec!["-y"],
        vec!["--yes"],
        vec!["--apply", "--force"],
        vec!["--apply", "--yes", "--kinds", "caches"],
    ] {
        let out = f.fixture_cmd().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(64), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(
            text_of(&out).1.contains("there is no force or yes flag"),
            "{args:?}: {}",
            text_of(&out).1
        );
    }
    assert_eq!(tree_manifest(f.root()), before);
}

#[test]
fn safety_mislabelled_mutating_cmd_refused() {
    let inner = FakeCommandRunner::new();
    inner.on(
        &["go", "env", "GOCACHE"],
        Ok(CmdOutput {
            stdout: b"/h/.cache/go-build\n".to_vec(),
            stderr: Vec::new(),
        }),
    );
    let runner = DryRunRunner(inner);
    // A mutating command labelled read-only, and a read-only-looking program with a mutating argument list.
    for (program, args) in [
        ("go", vec!["clean", "-cache"]),
        ("pip", vec!["cache", "purge"]),
        ("docker", vec!["volume", "rm", "v"]),
        ("rm", vec!["-rf", "/"]),
    ] {
        let cmd = Cmd::new(program, args.clone(), false);
        assert!(
            matches!(runner.run(&cmd), Err(DeleteError::Refused)),
            "{program} {args:?} labelled read-only was not refused in plan mode"
        );
    }
    // Control: a genuine read-only row passes through to the inner runner.
    let ro = Cmd::new("go", ["env", "GOCACHE"], false);
    assert_eq!(
        runner.run(&ro).unwrap().stdout_lossy().trim(),
        "/h/.cache/go-build",
        "vacuous: the read-only control did not pass through"
    );
    assert_eq!(
        runner.0.mutating_calls(),
        0,
        "plan mode ran a mutating command"
    );
}

fn policy(allowed: AllowedTarget, target: &str) -> DeletePolicy {
    DeletePolicy {
        root_dev: RootDev(1),
        root_mnt: None,
        home: PathBuf::from("/h"),
        home_id: Identity { dev: 1, ino: 1 },
        scan_roots: vec![PathBuf::from("/h/code")],
        allowed,
        target: PathBuf::from(target),
        require_sibling_any: vec![OsString::from("package.json")],
        max_depth: 64,
        restore_owner_write: false,
    }
}

#[test]
fn safety_floor_rule_both_sides() {
    let check = |p: &DeletePolicy| {
        let name = p.target.file_name().unwrap_or(std::ffi::OsStr::new(""));
        p.validate(name)
    };
    // Allowed side: a cache named exactly by its allowed path.
    for t in ["/h/.npm/_cacache", "/h/.cargo/registry/src"] {
        let p = policy(AllowedTarget::Exact(PathBuf::from(t)), t);
        assert_eq!(check(&p), Ok(()), "{t} must be allowed");
    }
    // Refused side: the home itself, its secrets, a scan root, a repo and its .git, and a path that is not the
    // allowed one even though both are caches.
    for t in ["/h", "/h/.ssh", "/h/Documents", "/h/.npm", "/"] {
        let p = policy(AllowedTarget::Exact(PathBuf::from(t)), t);
        assert!(check(&p).is_err(), "{t} must be refused");
    }
    for t in ["/h/code", "/h/code/repo", "/h/code/repo/.git"] {
        let p = policy(
            AllowedTarget::Within {
                root: PathBuf::from("/h/code"),
                name: NamePattern::NodeModules,
            },
            t,
        );
        assert!(check(&p).is_err(), "{t} must be refused");
    }
    let p = policy(
        AllowedTarget::Exact(PathBuf::from("/h/.npm/_cacache")),
        "/h/.gradle/caches",
    );
    assert!(check(&p).is_err(), "a path other than the allowed one");
}

fn dry_run_id(f: &Fixture) -> String {
    let out = f
        .fixture_cmd()
        .args(["--kinds", "fixture-destructive", "--json"])
        .output()
        .unwrap();
    common::extract_plan_id(&text_of(&out).0).expect("plan id")
}

#[test]
fn safety_destructive_apply_needs_plan_id() {
    let f = Fixture::new();
    populate(&f);
    let stale = f.home().join("code/proj/stale-a");
    let id = dry_run_id(&f);
    let no_id = f
        .fixture_cmd()
        .args(["--apply", "--kinds", "fixture-destructive"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(no_id.status.code(), Some(64), "{}", text_of(&no_id).1);
    assert!(no_id.stdout.is_empty());
    assert!(
        stale.is_dir(),
        "a destructive apply without --plan-id deleted"
    );
    // The gate decides on the tier of the resolved selector: another kind in the selection does not lift it.
    let mixed = f
        .fixture_cmd()
        .args(["--apply", "--kinds", "caches,fixture-destructive"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(mixed.status.code(), Some(64));
    assert!(stale.is_dir());
    // Control: with the reviewed id the same selection applies.
    let ok = f
        .fixture_cmd()
        .args([
            "--apply",
            "--kinds",
            "fixture-destructive",
            "--plan-id",
            &id,
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(ok.status.code(), Some(0), "{}", text_of(&ok).1);
    assert!(!stale.exists(), "vacuous: the control did not delete");
}

#[cfg(feature = "gate-b")]
#[test]
fn safety_out_of_root_cache_not_deleted() {
    let f = Fixture::new();
    populate(&f);
    let home = f.home();
    // A cache-looking directory that is not a row of the table, a cache that is a symlink to a directory
    // outside the cache roots, and the files the floor rule protects.
    std::fs::create_dir_all(home.join(".cache/not-in-the-table")).unwrap();
    std::fs::write(home.join(".cache/not-in-the-table/keep"), "k").unwrap();
    let elsewhere = home.join("Documents/elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("keep"), "k").unwrap();
    std::fs::create_dir_all(home.join(".gradle")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, home.join(".gradle/caches")).unwrap();
    let keep: Vec<PathBuf> = [
        ".cache/not-in-the-table/keep",
        "Documents/elsewhere/keep",
        "Documents/thesis.txt",
        ".ssh/id_ed25519",
        "code/proj/stale-a/f",
    ]
    .iter()
    .map(|p| home.join(p))
    .collect();
    let out = f
        .fixture_cmd()
        .args(["--apply", "--kinds", "caches"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    assert!(
        !home.join(".npm/_cacache").exists(),
        "vacuous: the apply removed nothing"
    );
    for p in &keep {
        assert!(p.exists(), "{} was deleted", p.display());
    }
    // An explicit --only outside the selected kinds' items is refused before anything runs.
    let only = f
        .fixture_cmd()
        .args(["--apply", "--kinds", "caches", "--only"])
        .arg(home.join("Documents"))
        .output()
        .unwrap();
    assert_eq!(only.status.code(), Some(64));
    assert!(home.join("Documents/thesis.txt").exists());
}

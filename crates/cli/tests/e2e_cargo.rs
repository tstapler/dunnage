#![cfg(feature = "gate-c")]
#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! The cargo-aware kinds end to end through the spawned binary under `fixture_cmd()`: `build-dirs` offers the
//! idle parts of a young cargo target (`cargo-artifact`), `--cargo-profile` keeps `release`, and `cargo-home`
//! removes aged crates below `CARGO_HOME`. Every directory lives in a tempdir fixture HOME; `audit verify`
//! must accept what the apply logged.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, SystemTime};

use common::{extract_plan_id, text_of, tree_manifest, Fixture};

const CARGO_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# created by cargo\n";

fn plain(f: &Fixture, args: &[&str]) -> Output {
    f.fixture_cmd()
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn age_tree(dir: &Path, days: u64) {
    let then = SystemTime::now() - Duration::from_hours(days * 24);
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if e.file_type().unwrap().is_dir() {
            age_tree(&p, days);
        } else {
            fs::File::open(&p).unwrap().set_modified(then).unwrap();
        }
    }
    fs::File::open(dir).unwrap().set_modified(then).unwrap();
}

fn profile(target: &Path, name: &str) {
    let p = target.join(name);
    for d in [
        ".fingerprint/foo-1",
        "deps",
        "build/foo-2",
        "incremental/foo-3",
    ] {
        fs::create_dir_all(p.join(d)).unwrap();
    }
    fs::write(p.join(".fingerprint/foo-1/invoked.timestamp"), "").unwrap();
    fs::write(p.join("deps/libfoo.rlib"), vec![7u8; 40_000]).unwrap();
    fs::write(p.join("build/foo-2/out"), "o").unwrap();
    fs::write(p.join("incremental/foo-3/dep-graph.bin"), vec![1u8; 40_000]).unwrap();
}

/// `~/code/app` with a cargo target whose `debug` is 40 days idle and whose `release` was built just now.
fn project(f: &Fixture) -> PathBuf {
    let proj = f.home().join("code/app");
    fs::create_dir_all(proj.join("src")).unwrap();
    fs::write(proj.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
    let t = proj.join("target");
    fs::create_dir_all(&t).unwrap();
    fs::write(t.join("CACHEDIR.TAG"), CARGO_TAG).unwrap();
    profile(&t, "debug");
    profile(&t, "release");
    age_tree(&t.join("debug"), 40);
    let then = SystemTime::now() - Duration::from_hours(40 * 24);
    fs::File::open(t.join("CACHEDIR.TAG"))
        .unwrap()
        .set_modified(then)
        .unwrap();
    t
}

fn json(out: &Output) -> serde_json::Value {
    assert_eq!(out.status.code(), Some(0), "{}", text_of(out).1);
    serde_json::from_slice(&out.stdout).unwrap()
}

fn items<'a>(doc: &'a serde_json::Value, kind: &str) -> Vec<&'a serde_json::Value> {
    doc["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["kind"] == kind)
        .collect()
}

#[test]
fn kinds_should_list_cargo_home_as_a_guarded_kind_that_can_delete() {
    let f = Fixture::new();
    let doc = json(&plain(&f, &["kinds", "--json"]));
    let row = doc["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == "cargo-home")
        .expect("cargo-home is listed");
    assert_eq!(row["tier"], "guarded");
    assert_eq!(row["available"], "yes");
    let bd = doc["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == "build-dirs")
        .unwrap();
    assert!(bd["touches"].as_str().unwrap().contains("cargo target"));
}

#[test]
fn a_dry_run_should_offer_the_debug_parts_of_a_young_target_and_delete_nothing() {
    let f = Fixture::new();
    let t = project(&f);
    let before = tree_manifest(&f.home());

    let doc = json(&plain(
        &f,
        &[
            "--kinds",
            "build-dirs",
            "--cargo-profile",
            "debug",
            "--json",
        ],
    ));

    let parts = items(&doc, "cargo-artifact");
    assert_eq!(parts.len(), 4, "{parts:?}");
    for p in &parts {
        assert_eq!(p["selector"], "build-dirs");
        assert_eq!(p["family"], "build-dirs");
        assert_eq!(p["tier"], "destructive");
        assert!(p["path"].as_str().unwrap().contains("target/debug/"));
    }
    assert!(doc["next"].as_array().unwrap().iter().any(|n| n["command"]
        .as_str()
        .unwrap()
        .contains("--cargo-profile debug")));
    assert_eq!(tree_manifest(&f.home()), before);
    assert!(t.join("debug/deps/libfoo.rlib").exists());
}

#[test]
fn apply_should_need_the_plan_id_and_then_drop_only_the_debug_parts_and_audit_verify_should_accept_it(
) {
    let f = Fixture::new();
    let t = project(&f);
    let args = ["--kinds", "build-dirs", "--cargo-profile", "debug"];

    let refused = plain(
        &f,
        &[
            "--apply",
            "--kinds",
            "build-dirs",
            "--cargo-profile",
            "debug",
        ],
    );
    assert_eq!(refused.status.code(), Some(64));
    assert!(t.join("debug/deps/libfoo.rlib").exists());

    let mut dry = args.to_vec();
    dry.push("--json");
    let id = extract_plan_id(&text_of(&plain(&f, &dry)).0).expect("plan id");
    let mut apply = vec!["--apply", "--plan-id", &id];
    apply.extend_from_slice(&args);
    let out = plain(&f, &apply);
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);

    for part in ["deps", ".fingerprint", "build", "incremental"] {
        assert!(!t.join("debug").join(part).exists(), "{part}");
    }
    assert!(t.join("release/deps/libfoo.rlib").exists());
    assert!(t.join("CACHEDIR.TAG").exists());
    assert!(f.home().join("code/app/Cargo.toml").exists());

    let verify = plain(&f, &["audit", "verify"]);
    assert_eq!(verify.status.code(), Some(0), "{}", text_of(&verify).0);
    let log = fs::read_to_string(f.xdg("state").join("dunnage/audit.jsonl")).unwrap();
    assert!(log.contains("\"cargo-artifact\""), "{log}");
}

#[test]
fn cargo_profile_should_be_a_usage_error_without_build_dirs_in_kinds() {
    let f = Fixture::new();
    let out = plain(&f, &["--kinds", "caches", "--cargo-profile", "debug"]);
    assert_eq!(out.status.code(), Some(64));
    assert!(text_of(&out).1.contains("--cargo-profile"));
}

#[test]
fn cargo_home_should_apply_without_a_plan_id_follow_cargo_home_and_pass_audit_verify() {
    let f = Fixture::new();
    let ch = f.home().join("dev/cargo");
    let idx = ch.join("registry/src/index.crates.io-6f17d22bba15001f");
    let old = idx.join("serde-1.0.200");
    let young = idx.join("tokio-1.40.0");
    let cache = ch.join("registry/cache/index.crates.io-6f17d22bba15001f/serde-1.0.200.crate");
    for (d, days) in [(&old, 60), (&young, 2)] {
        fs::create_dir_all(d.join("src")).unwrap();
        fs::write(d.join("src/lib.rs"), vec![b'x'; 20_000]).unwrap();
        age_tree(d, days);
    }
    fs::create_dir_all(cache.parent().unwrap()).unwrap();
    fs::write(&cache, "crate").unwrap();
    let envd = |f: &Fixture, args: &[&str]| {
        f.fixture_cmd()
            .env("CARGO_HOME", &ch)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };

    let doc = json(&envd(&f, &["--kinds", "cargo-home", "--json"]));
    let got = items(&doc, "cargo-home");
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["path"], old.to_str().unwrap());
    assert_eq!(got[0]["tier"], "guarded");

    let out = envd(&f, &["--apply", "--kinds", "cargo-home"]);
    assert_eq!(out.status.code(), Some(0), "{}", text_of(&out).1);
    assert!(!old.exists());
    assert!(young.join("src/lib.rs").exists());
    assert!(cache.exists());
    assert!(idx.is_dir());

    let verify = plain(&f, &["audit", "verify"]);
    assert_eq!(verify.status.code(), Some(0), "{}", text_of(&verify).0);
}

#[test]
fn cargo_home_outside_home_should_list_nothing() {
    let f = Fixture::new();
    let doc = {
        let out = f
            .fixture_cmd()
            .env("CARGO_HOME", "/opt/cargo")
            .args(["--kinds", "cargo-home", "--json"])
            .output()
            .unwrap();
        json(&out)
    };
    assert!(items(&doc, "cargo-home").is_empty());
}

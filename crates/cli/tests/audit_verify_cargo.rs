#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! `audit verify` and the cargo-aware kinds: the reviewed table must accept exactly the paths the engine can
//! delete for `cargo-artifact` and `cargo-home`, and flag everything else.

use std::path::{Path, PathBuf};

use dunnage_adapters::audit_verify::{
    verify_audit, verify_audit_with, AllowedTable, VerifyExtras, VerifyInputs,
};
use serde_json::json;

const HOME: &str = "/home/u";

fn start(run: &str) -> String {
    json!({"type":"run_start","schema":1,"run_id":run,"ts":1,"roots":{
        "scan_roots":["/home/u/code"],"cache_paths":[],"worktree_roots":["/home/u/code"],"steam_libraries":[]}})
    .to_string()
}

fn removed(run: &str, kind: &str, path: &str) -> String {
    json!({"type":"result","schema":1,"run_id":run,"ts":2,"kind":kind,"status":"removed","path":path}).to_string()
}

fn violations(kind: &str, paths: &[&str], config: Option<&[PathBuf]>) -> Vec<String> {
    let mut lines = vec![start("r")];
    lines.extend(paths.iter().map(|p| removed("r", kind, p)));
    let table = AllowedTable::embedded();
    let text = lines.join("\n");
    verify_audit(
        text.as_bytes(),
        &VerifyInputs {
            table: &table,
            home: Path::new(HOME),
            config_roots: config,
        },
    )
    .unwrap()
    .violations
    .into_iter()
    .map(|v| v.path)
    .collect()
}

#[test]
fn audit_verify_should_accept_every_part_of_a_target_below_a_default_root_and_flag_the_rest() {
    let ok = [
        "/home/u/code/app/target/debug/incremental",
        "/home/u/code/app/target/release/deps",
        "/home/u/code/app/target/debug/.fingerprint",
        "/home/u/code/app/target/debug/build",
        "/home/u/code/app/target/debug/examples",
        "/home/u/code/app/target/aarch64-apple-darwin/release/incremental",
    ];
    assert!(violations("cargo-artifact", &ok, None).is_empty());
    let bad = [
        "/home/u/code/app/target/debug",
        "/home/u/code/app/target",
        "/home/u/code/app/target/debug/out",
        "/home/u/code/app/src/incremental",
        "/home/u/elsewhere/app/target/debug/incremental",
        "/home/u/code/app/target/debug/../../src/deps",
    ];
    assert_eq!(violations("cargo-artifact", &bad, None), bad);
}

#[test]
fn audit_verify_should_accept_a_cargo_part_below_a_configured_root() {
    let cfg = [PathBuf::from("/data/work")];
    assert!(violations(
        "cargo-artifact",
        &["/data/work/x/target/debug/deps"],
        Some(&cfg)
    )
    .is_empty());
}

#[test]
fn audit_verify_should_accept_a_cargo_home_entry_below_the_default_cargo_home_only() {
    let ok = [
        "/home/u/.cargo/registry/src/index.crates.io-6f17d22bba15001f/serde-1.0.200",
        "/home/u/.cargo/git/checkouts/repo-1a2b3c/abcdef1",
    ];
    assert!(violations("cargo-home", &ok, None).is_empty());
    let bad = [
        "/home/u/.cargo/registry/src",
        "/home/u/.cargo/registry/src/idx",
        "/home/u/.cargo/registry/cache/idx/serde-1.0.200",
        "/home/u/.cargo/git/db/repo-abc",
        "/home/u/.cargo/bin/cargo",
        "/opt/cargo/registry/src/idx/serde-1.0.200",
        "/registry/src/idx/serde-1.0.200",
        "/home/u/registry/src/idx/serde-1.0.200",
        "/home/u/.cargo/registry/src/idx/serde-1.0.200/src",
        "/home/u/.cargo/registry/src/idx/.git",
        "/home/u/x/.git/registry/src/idx/serde-1.0.200",
        // a moved CARGO_HOME is only known when the verifier is told
        "/home/u/dev/cargo/registry/src/index.crates.io-6f17d22bba15001f/serde-1.0.200",
    ];
    assert_eq!(violations("cargo-home", &bad, None), bad);
}

fn violations_with_cargo_home(paths: &[&str], cargo_home: &str) -> Vec<String> {
    let mut lines = vec![start("r")];
    lines.extend(paths.iter().map(|p| removed("r", "cargo-home", p)));
    let table = AllowedTable::embedded();
    verify_audit_with(
        lines.join("\n").as_bytes(),
        &VerifyInputs {
            table: &table,
            home: Path::new(HOME),
            config_roots: None,
        },
        &VerifyExtras {
            cargo_home: Some(PathBuf::from(cargo_home)),
            ..VerifyExtras::default()
        },
    )
    .unwrap()
    .violations
    .into_iter()
    .map(|v| v.path)
    .collect()
}

#[test]
fn audit_verify_should_accept_a_cargo_home_entry_below_the_cargo_home_the_verifier_was_given() {
    let moved = "/home/u/dev/cargo/registry/src/index.crates.io-6f17d22bba15001f/serde-1.0.200";
    let default = "/home/u/.cargo/git/checkouts/repo-1a2b3c/abcdef1";
    assert!(violations_with_cargo_home(&[moved, default], "/home/u/dev/cargo").is_empty());
    // a CARGO_HOME that is $HOME, outside it, unclean or relative adds nothing
    for bad in [
        "/home/u",
        "/opt/cargo",
        "/home/u/../u/dev/cargo",
        "dev/cargo",
    ] {
        assert_eq!(
            violations_with_cargo_home(&[moved], bad),
            [moved],
            "CARGO_HOME={bad}"
        );
    }
    // and nothing else below home is widened by it
    let other = "/home/u/Documents/registry/src/a/b";
    assert_eq!(
        violations_with_cargo_home(&[other], "/home/u/dev/cargo"),
        [other]
    );
}

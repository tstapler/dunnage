#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! `cli/build.rs` re-bakes the commit after a new commit and records dirty and scan-dirty (Story 2.2.1,
//! Task 2.2.1d; validation T-497). The scratch crate holds a verbatim copy of the real `build.rs`, in a git repo
//! of its own under the system temp dir, built with a target dir outside it.

use std::path::Path;
use std::process::Command;

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Builds the scratch crate and returns what its binary prints: `commit dirty scan_rev scan_dirty`.
fn baked(repo: &Path, target: &Path) -> Vec<String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let build = Command::new(&cargo)
        .args(["run", "-q", "--offline"])
        .current_dir(repo)
        .env("CARGO_TARGET_DIR", target)
        .env_remove("RUSTC_WRAPPER")
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    String::from_utf8_lossy(&build.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

#[test]
fn build_rs_should_rebake_the_commit_after_a_new_commit_and_record_dirty_and_metascope_dirty() {
    if !have_git() {
        eprintln!("skip: git is not installed");
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let r = repo.path();
    std::fs::create_dir_all(r.join("src")).unwrap();
    std::fs::create_dir_all(r.join("crates/scan")).unwrap();
    std::fs::write(
        r.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(r.join("build.rs"), include_str!("../build.rs")).unwrap();
    std::fs::write(
        r.join("src/main.rs"),
        "fn main() { println!(\"{} {} {} {}\", env!(\"DC_GIT_COMMIT\"), env!(\"DC_GIT_DIRTY\"), env!(\"DC_SCAN_REV\"), env!(\"DC_SCAN_DIRTY\")); }\n",
    )
    .unwrap();
    std::fs::write(r.join("crates/scan/lib.txt"), "one\n").unwrap();
    std::fs::write(r.join(".gitignore"), "target\nCargo.lock\n").unwrap();
    git(r, &["init", "-q", "-b", "main"]);
    git(r, &["add", "."]);
    git(r, &["commit", "-q", "-m", "c1"]);
    let c1 = git(r, &["rev-parse", "HEAD"]);

    let got = baked(r, target.path());
    assert_eq!(
        got,
        [c1.as_str(), "false", c1.as_str(), "false"],
        "built at C1"
    );

    git(r, &["commit", "-q", "--allow-empty", "-m", "c2"]);
    let c2 = git(r, &["rev-parse", "HEAD"]);
    assert_ne!(c1, c2);
    let got = baked(r, target.path());
    assert_eq!(
        got,
        [c2.as_str(), "false", c1.as_str(), "false"],
        "rebuilt after an empty commit"
    );

    std::fs::write(r.join("crates/scan/lib.txt"), "two\n").unwrap();
    git(r, &["add", "crates/scan/lib.txt"]);
    let got = baked(r, target.path());
    assert_eq!(
        got,
        [c2.as_str(), "true", c1.as_str(), "true"],
        "rebuilt after a staged scan change"
    );
}

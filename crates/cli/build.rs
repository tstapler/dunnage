//! Bakes the build's provenance into the binary (Story 2.2.1, Task 2.2.1d): the commit, whether the tree was
//! dirty, the last commit that touched `crates/scan` and whether that directory was dirty. Outside a git
//! checkout every value is `unknown`/`false`. The rerun inputs below are what keep the baked commit from going
//! stale after a commit that changes no source file.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn absolute(base: &Path, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

fn main() {
    let manifest =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()));
    let top = git(&manifest, &["rev-parse", "--show-toplevel"]).map(PathBuf::from);
    let commit = git(&manifest, &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&manifest, &["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    let (scan_rev, scan_dirty) = match &top {
        Some(top) => (
            git(top, &["log", "-1", "--format=%H", "--", "crates/scan"])
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| commit.clone()),
            git(top, &["status", "--porcelain", "--", "crates/scan"])
                .is_some_and(|s| !s.is_empty()),
        ),
        None => ("unknown".into(), false),
    };
    println!("cargo:rustc-env=DC_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=DC_GIT_DIRTY={dirty}");
    println!("cargo:rustc-env=DC_SCAN_REV={scan_rev}");
    println!("cargo:rustc-env=DC_SCAN_DIRTY={scan_dirty}");

    // HEAD and the index live in this worktree's git dir; refs and packed-refs in the common dir.
    let git_dir = git(&manifest, &["rev-parse", "--git-dir"]).map(|d| absolute(&manifest, &d));
    let common =
        git(&manifest, &["rev-parse", "--git-common-dir"]).map(|d| absolute(&manifest, &d));
    if let Some(gd) = &git_dir {
        println!("cargo:rerun-if-changed={}", gd.join("HEAD").display());
        println!("cargo:rerun-if-changed={}", gd.join("index").display());
        let common = common.as_deref().unwrap_or(gd);
        println!(
            "cargo:rerun-if-changed={}",
            common.join("packed-refs").display()
        );
        if let Some(r) = git(&manifest, &["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={}", common.join(r).display());
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    // Edits that are neither committed nor staged still flip `dirty`; watching the source trees reruns this.
    if let Some(top) = &top {
        println!(
            "cargo:rerun-if-changed={}",
            top.join("crates/scan").display()
        );
    }
}

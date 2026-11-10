//! A size-limited tmpfs in a private mount namespace (`unshare -rm`, no root needed), for the full-disk legs.
//! The parent test re-runs a child test of the same binary inside the namespace; the child finds the mount
//! point in `DC_FULLFS_DIR`. Where `unshare -rm` is not permitted the leg is a named skip, and
//! `DC_REQUIRE_TMPFS_FILL=1` turns the skip into a failure (the release checklist sets it).

use std::fs::OpenOptions;
use std::io::Write;
use std::process::Command;

pub const DIR_VAR: &str = "DC_FULLFS_DIR";

/// Runs `child_test` (an `#[test]` in this binary that returns early without `DIR_VAR`) inside a tmpfs of
/// `size_kib`. Returns false when the leg was skipped.
pub fn run_child(child_test: &str, size_kib: u32) -> bool {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = Command::new("unshare")
        .args(["-rm", "sh", "-c"])
        .arg(format!(
            "mount -t tmpfs -o size={size_kib}k tmpfs \"$1\" && shift && exec \"$@\""
        ))
        .arg("sh")
        .arg(dir.path())
        .arg(std::env::current_exe().expect("exe"))
        .args(["--exact", child_test, "--nocapture", "--test-threads=1"])
        .env(DIR_VAR, dir.path())
        .output();
    let marker = format!("CHILD_OK:{child_test}");
    match out {
        Ok(o) if String::from_utf8_lossy(&o.stdout).contains(&marker) => true,
        Ok(o)
            if o.status.code() == Some(1)
                && !String::from_utf8_lossy(&o.stdout).contains("panicked")
                && String::from_utf8_lossy(&o.stderr).contains("mount") =>
        {
            skip(child_test, "mount refused")
        }
        Ok(o) if o.status.success() => panic!(
            "child {child_test} exited 0 without its marker:\n{}",
            String::from_utf8_lossy(&o.stdout)
        ),
        Ok(o) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            if text.contains("running 1 test") {
                panic!("child {child_test} failed:\n{text}");
            }
            skip(child_test, "unshare -rm not permitted")
        }
        Err(_) => skip(child_test, "unshare not installed"),
    }
}

fn skip(test: &str, why: &str) -> bool {
    assert!(
        std::env::var_os("DC_REQUIRE_TMPFS_FILL").is_none(),
        "{test}: tmpfs leg required but skipped ({why})"
    );
    eprintln!("skip: {test}: {why}");
    false
}

/// Writes `fill` until the filesystem reports `ENOSPC`.
pub fn fill(dir: &std::path::Path) {
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dir.join("fill"))
        .expect("fill file");
    let chunk = [0u8; 4096];
    while f.write(&chunk).is_ok_and(|n| n > 0) {}
}

pub fn child_dir() -> Option<std::path::PathBuf> {
    std::env::var_os(DIR_VAR).map(Into::into)
}

pub fn ok(test: &str) {
    println!("CHILD_OK:{test}");
}

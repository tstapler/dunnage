#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Bounded descriptors (Story 2.1.2, T-530): a 10,000-level chain is deleted under a soft `RLIMIT_NOFILE` of 256.
//! The test re-executes itself under `ulimit -Sn 256`; tempdirs only.

use std::ffi::OsString;
use std::fs;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use dunnage_adapters::dirops::MAX_OPEN_LEVELS;
use dunnage_adapters::testing::{Event, FaultDirOps};
use dunnage_adapters::{delete_tree, open_parent, open_verified_root, RealDirOps};
use dunnage_core::{
    AbortFlag, AllowedTarget, ApplyToken, DeletePolicy, Identity, NamePattern, RootDev,
};
use rustix::fs::{mkdirat, openat, Mode, OFlags};

const LEVELS: usize = 10_000;
const CHILD_ENV: &str = "DC_FDLIMIT_CHILD";
const TEST_NAME: &str = "ten_thousand_levels_should_delete_with_soft_nofile_256_and_never_hold_more_than_max_open_levels_plus_16_descriptors";

fn ident(p: &Path) -> Identity {
    Identity::of(&fs::symlink_metadata(p).unwrap())
}

fn open_dirfd(parent: Option<&OwnedFd>, name: &str) -> OwnedFd {
    let flags = OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::RDONLY;
    match parent {
        Some(p) => openat(p, name, flags, Mode::empty()).unwrap(),
        None => rustix::fs::open(name, flags, Mode::empty()).unwrap(),
    }
}

/// Chain of `LEVELS` directories built with `mkdirat` + `openat` so no path exceeds `PATH_MAX`.
fn build_chain(top: &Path) {
    fs::create_dir(top).unwrap();
    let mut cur = open_dirfd(None, top.to_str().unwrap());
    for _ in 0..LEVELS {
        mkdirat(&cur, "d", Mode::from_raw_mode(0o755)).unwrap();
        cur = open_dirfd(Some(&cur), "d");
    }
}

fn fd_count() -> usize {
    fs::read_dir("/proc/self/fd").unwrap().count()
}

fn run_in_child() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let nm = base.join("home/code/app/node_modules");
    fs::create_dir_all(nm.parent().unwrap()).unwrap();
    fs::write(base.join("home/code/app/package.json"), "{}").unwrap();
    build_chain(&nm);

    let home = base.join("home");
    let policy = DeletePolicy {
        root_dev: RootDev(fs::metadata(&base).unwrap().dev()),
        root_mnt: None,
        home: home.clone(),
        home_id: ident(&home),
        scan_roots: vec![home.join("code")],
        allowed: AllowedTarget::Within {
            root: home.join("code"),
            name: NamePattern::NodeModules,
        },
        target: nm.clone(),
        require_sibling_any: vec![OsString::from("package.json")],
        max_depth: 100_000,
        restore_owner_write: false,
    };

    let max_seen = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&max_seen);
    let ops = FaultDirOps::wrap(RealDirOps::confined_to(&base))
        .hook(Event::BeforeReadDir, move |_| {
            seen.fetch_max(fd_count(), Ordering::SeqCst);
        })
        .build();
    let root = open_verified_root(&ops, &base, ident(&base)).unwrap();
    let parent = open_parent(&ops, &root, Path::new("home/code/app")).unwrap();
    let id = ident(&nm);
    let result = delete_tree(
        &ops,
        &parent,
        "node_modules".as_ref(),
        id,
        &policy,
        &AbortFlag::new(),
        &ApplyToken::for_tests(),
    );
    result.expect("delete_tree must succeed under a 256-descriptor soft limit");
    assert!(fs::symlink_metadata(&nm).is_err(), "chain must be gone");
    let peak = max_seen.load(Ordering::SeqCst);
    assert!(peak > 0, "the BeforeReadDir hook never fired");
    assert!(
        peak <= MAX_OPEN_LEVELS + 16,
        "peak open descriptors {peak} exceeds MAX_OPEN_LEVELS + 16 = {}",
        MAX_OPEN_LEVELS + 16
    );
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "10 000 nested levels take about 16 minutes on APFS (965 s measured on a macos-15 runner): Linux only"
)]
fn ten_thousand_levels_should_delete_with_soft_nofile_256_and_never_hold_more_than_max_open_levels_plus_16_descriptors(
) {
    if std::env::var_os(CHILD_ENV).is_some() {
        run_in_child();
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let out = Command::new("sh")
        .arg("-c")
        .arg(r#"ulimit -Sn 256 && test "$(ulimit -Sn)" = 256 && exec "$0" --exact "$1" --nocapture --test-threads=1"#)
        .arg(&exe)
        .arg(TEST_NAME)
        .env(CHILD_ENV, "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "child failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("1 passed"),
        "child did not run the test: {stdout}"
    );
}

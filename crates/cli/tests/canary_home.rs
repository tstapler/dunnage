#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 2.1.6 fixture-HOME canary. A sentinel file cannot catch a `getpwuid` regression; the syscall trace
//! can, because the passwd lookup opens `/etc/passwd` or `/etc/nsswitch.conf`.

mod common;

use std::path::{Path, PathBuf};

use common::{
    canary_violations, strace_available, strace_cmd, strace_own_paths, tree_manifest, Fixture,
};

/// Paths a plan run may open that are neither the fixture nor its root: the loader and libc, `/proc/self`,
/// the cgroup quota files Rust's `available_parallelism` reads, the btrfs sysfs tree, devices.
const SYSTEM_PREFIXES: [&str; 8] = [
    "/usr",
    "/lib",
    "/lib64",
    "/proc/self",
    "/sys/fs/btrfs",
    "/sys/fs/cgroup",
    "/dev",
    "/bin",
];
const SYSTEM_EXACT: [&str; 2] = ["/etc/ld.so.cache", "/etc/ld.so.preload"];

fn skip_without_strace() -> bool {
    if strace_available() {
        return false;
    }
    assert!(
        std::env::var_os("DC_REQUIRE_STRACE").is_none(),
        "DC_REQUIRE_STRACE is set and strace is not installed"
    );
    eprintln!("SKIP: strace is not installed (the canary is a named skip; DC_REQUIRE_STRACE=1 makes it a failure)");
    true
}

fn allowlist(fx: &Fixture) -> Vec<PathBuf> {
    let mut a: Vec<PathBuf> = SYSTEM_PREFIXES.iter().map(PathBuf::from).collect();
    a.push(std::fs::canonicalize(fx.root()).unwrap());
    a.push(fx.root().to_path_buf());
    let exe = Path::new(env!("CARGO_BIN_EXE_disk-cleanup"));
    a.push(exe.parent().unwrap().to_path_buf());
    a
}

/// A PATH with nothing in it that belongs to the developer, so the run is the same on every machine.
fn fixture_path(fx: &Fixture) -> std::ffi::OsString {
    let bin = fx.root().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    common::docker_down_shim(&bin);
    std::env::join_paths([bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]).unwrap()
}

#[test]
fn canary_should_fail_when_the_trace_shows_a_passwd_lookup_or_a_path_outside_the_fixture() {
    if skip_without_strace() {
        return;
    }
    let fx = Fixture::new();
    // Control: a real traced program that reads the files a getpwuid regression reads.
    for target in ["/etc/passwd", "/etc/nsswitch.conf"] {
        let log = fx.root().join("control.log");
        let out = strace_cmd(&fx, &log, "cat").arg(target).output().unwrap();
        assert!(out.status.success(), "control run: {out:?}");
        let paths = strace_own_paths(&std::fs::read_to_string(&log).unwrap());
        let bad = canary_violations(&paths, &allowlist(&fx), &SYSTEM_EXACT);
        assert!(
            bad.iter().any(|p| p == target),
            "{target} not flagged: {bad:?}"
        );
    }
    // A subprocess's own reads are not the binary's (it has its own rules); a thread of the root process is.
    let synthetic = "100 execve(\"/bin/x\", [], []) = 0\n\
        100 openat(AT_FDCWD, \"/home/real/.cache/a\", O_RDONLY) = 3\n\
        101 execve(\"/usr/bin/go\", [], []) = 0\n\
        101 openat(AT_FDCWD, \"/etc/passwd\", O_RDONLY) = 3\n\
        102 newfstatat(AT_FDCWD, \"/home/real/.ssh\", {}, 0) = 0\n\
        100 openat(3, \"relative\", O_RDONLY) = 4\n";
    assert_eq!(
        strace_own_paths(synthetic),
        vec![
            "/bin/x".to_string(),
            "/home/real/.cache/a".to_string(),
            "/home/real/.ssh".to_string()
        ]
    );
    // Pure cases.
    let a = allowlist(&fx);
    let inside = fx.home().join("x").to_string_lossy().into_owned();
    assert!(
        canary_violations(&[inside, "/usr/lib/libc.so.6".into()], &a, &SYSTEM_EXACT).is_empty()
    );
    assert_eq!(
        canary_violations(&["/home/someone/.cache/x".into()], &a, &SYSTEM_EXACT),
        vec!["/home/someone/.cache/x".to_string()]
    );
}

#[test]
fn canary_should_show_a_plan_run_touching_only_the_fixture_home_and_allowlisted_system_paths() {
    if skip_without_strace() {
        return;
    }
    let fx = Fixture::new();
    let log = fx.root().join("plan.log");
    let mut c = strace_cmd(&fx, &log, env!("CARGO_BIN_EXE_disk-cleanup"));
    c.arg("--json")
        .env("PATH", fixture_path(&fx))
        .current_dir(fx.home());
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(&log).unwrap();
    let paths = strace_own_paths(&text);
    assert!(
        paths
            .iter()
            .any(|p| p.starts_with(fx.root().to_str().unwrap()) || p.contains("xdg")),
        "vacuous trace: the run opened nothing under the fixture ({} paths)",
        paths.len()
    );
    // The ancestors of the fixture root are stat'ed when it is canonicalised: exact entries, not prefixes.
    let ancestors: Vec<String> = std::fs::canonicalize(fx.root())
        .unwrap()
        .ancestors()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let exact: Vec<&str> = SYSTEM_EXACT
        .iter()
        .copied()
        .chain(ancestors.iter().map(String::as_str))
        .collect();
    // Snapshot detection (Story 4.3.1) lstats `<mount>/.snapshots` for every mount in the table; a leaf of that
    // exact name is a read-only probe, not a home access. Probing only btrfs mounts would narrow it (MountEntry
    // carries no fstype yet).
    let bad: Vec<String> = canary_violations(&paths, &allowlist(&fx), &exact)
        .into_iter()
        .filter(|p| !p.ends_with("/.snapshots") && p != "/timeshift-btrfs/snapshots")
        .collect();
    assert!(
        bad.is_empty(),
        "paths outside the fixture and the allowlist: {bad:#?}"
    );
}

#[test]
fn a_scan_root_with_a_symlink_to_the_real_home_should_leave_everything_outside_the_fixture_unchanged(
) {
    let fx = Fixture::new();
    // A stand-in for the owner's home, outside DC_TEST_ROOT: the test never points a run at the real one.
    let real_home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(real_home.path().join(".cache/pip")).unwrap();
    std::fs::write(real_home.path().join(".cache/pip/blob"), "data").unwrap();
    std::fs::write(real_home.path().join("notes.txt"), "keep").unwrap();
    let root = fx.home().join("projects");
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(real_home.path(), root.join("home-link")).unwrap();
    let before = tree_manifest(real_home.path());

    let out = fx
        .fixture_cmd()
        .current_dir(fx.home())
        .args(["--json", "--root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(tree_manifest(real_home.path()), before);
    assert!(real_home.path().join(".cache/pip/blob").is_file());
}

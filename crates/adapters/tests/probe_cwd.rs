#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! `SystemProbe::cwd_in_use` (Story 3.1.2, triad U-1): a process whose cwd is below the path, or that holds
//! an open file below it, is a holder. Real processes on Linux, a fake `/proc` for the failure shapes, and a
//! `FakeCommandRunner` for the macOS `lsof` branch.

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use disk_cleanup_adapters::probe::cwd::{lsof_scan, scan_proc};
use disk_cleanup_adapters::RealProbe;
use disk_cleanup_core::ports::CmdOutput;
use disk_cleanup_core::ports::SystemProbe;
use disk_cleanup_core::testing::FakeCommandRunner;
use disk_cleanup_core::{DeleteError, Tri};

struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = fs::canonicalize(t.path()).unwrap();
    (t, p)
}

fn sleeper(cwd: &Path, stdin: Stdio) -> Reaped {
    Reaped(
        Command::new("sleep")
            .arg("60")
            .current_dir(cwd)
            .stdin(stdin)
            .spawn()
            .unwrap(),
    )
}

/// Asks `cwd_in_use(path)` until it names `pid` (for up to 5 s). The scan reports the first holder it meets, and a
/// sibling test thread that forks while this test holds a descriptor below `path` leaves a child that holds a copy
/// of it, named after the test binary, until its `exec`; that holder is gone a moment later, the one under test
/// stays.
#[cfg(target_os = "linux")]
fn names_holder(path: &Path, pid: u32) -> (Tri, Option<(String, u32)>) {
    let mut last = RealProbe.cwd_in_use(path);
    for _ in 0..500 {
        if last.1.as_ref().is_some_and(|h| h.1 == pid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        last = RealProbe.cwd_in_use(path);
    }
    last
}

#[cfg(target_os = "linux")]
#[test]
fn cwd_in_use_should_name_a_process_whose_cwd_is_the_worktree_or_below_it() {
    let (_t, p) = tmp();
    let wt = p.join("wt");
    let sub = wt.join("a/b");
    fs::create_dir_all(&sub).unwrap();
    for cwd in [&wt, &sub] {
        let child = sleeper(cwd, Stdio::null());
        let (tri, holder) = names_holder(&wt, child.0.id());
        assert_eq!(tri, Tri::Running, "cwd {cwd:?}");
        let (name, pid) = holder.unwrap();
        // the scan may meet another holder first only if one exists; none does in a fresh tempdir
        assert_eq!(pid, child.0.id());
        assert_eq!(name, "sleep");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cwd_in_use_should_count_an_open_file_below_the_worktree() {
    let (_t, p) = tmp();
    let wt = p.join("wt");
    let elsewhere = p.join("elsewhere");
    fs::create_dir_all(&wt).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(wt.join("data.sqlite"), b"x").unwrap();
    let file = fs::File::open(wt.join("data.sqlite")).unwrap();
    let child = sleeper(&elsewhere, Stdio::from(file));
    let (tri, holder) = names_holder(&wt, child.0.id());
    assert_eq!(tri, Tri::Running);
    assert_eq!(holder.unwrap().1, child.0.id());
}

#[cfg(target_os = "linux")]
#[test]
fn cwd_in_use_should_not_name_a_process_in_a_sibling_with_the_same_prefix() {
    let (_t, p) = tmp();
    let wt = p.join("agent-1");
    let sibling = p.join("agent-10");
    fs::create_dir_all(&wt).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    let _child = sleeper(&sibling, Stdio::null());
    let (tri, holder) = RealProbe.cwd_in_use(&wt);
    assert_ne!(tri, Tri::Running, "path components, not string prefixes");
    assert_eq!(holder, None);
}

#[cfg(target_os = "linux")]
#[test]
fn cwd_in_use_should_follow_a_symlinked_argument_to_the_real_directory() {
    let (_t, p) = tmp();
    let real = p.join("real");
    fs::create_dir_all(&real).unwrap();
    symlink(&real, p.join("link")).unwrap();
    let child = sleeper(&real, Stdio::null());
    let (tri, holder) = names_holder(&p.join("link"), child.0.id());
    assert_eq!(tri, Tri::Running);
    assert_eq!(holder.unwrap().1, child.0.id());
}

fn fake_proc(root: &Path, pid: u32, cwd: Option<&Path>, fds: &[&Path]) -> PathBuf {
    let d = root.join(pid.to_string());
    fs::create_dir_all(d.join("fd")).unwrap();
    fs::write(d.join("comm"), "claude\n").unwrap();
    if let Some(c) = cwd {
        symlink(c, d.join("cwd")).unwrap();
    }
    for (i, f) in fds.iter().enumerate() {
        symlink(f, d.join("fd").join(i.to_string())).unwrap();
    }
    d
}

#[test]
fn scan_proc_should_find_a_cwd_or_fd_holder_and_ignore_other_entries() {
    let (_t, p) = tmp();
    let wt = p.join("wt");
    fs::create_dir_all(&wt).unwrap();
    let proc_root = p.join("proc");
    fs::create_dir_all(proc_root.join("self")).unwrap();
    fs::write(proc_root.join("cpuinfo"), "").unwrap();
    fake_proc(&proc_root, 10, Some(&p), &[]);
    assert_eq!(scan_proc(&proc_root, &wt, 1000), (Tri::NotRunning, None));
    fake_proc(&proc_root, 20, Some(&wt.join("sub")), &[]);
    assert_eq!(
        scan_proc(&proc_root, &wt, 1000),
        (Tri::Running, Some(("claude".into(), 20)))
    );
    let (_t2, q) = tmp();
    let proc2 = q.join("proc");
    fs::create_dir_all(&proc2).unwrap();
    fake_proc(&proc2, 30, Some(&q), &[&wt.join(".env")]);
    assert_eq!(
        scan_proc(&proc2, &wt, 1000),
        (Tri::Running, Some(("claude".into(), 30)))
    );
}

#[test]
fn scan_proc_should_be_unknown_when_proc_cannot_be_read() {
    let (_t, p) = tmp();
    assert_eq!(
        scan_proc(&p.join("no-such-proc"), &p, 1000),
        (Tri::Unknown, None)
    );
}

#[test]
fn scan_proc_should_be_unknown_for_an_unreadable_process_of_our_user_but_not_another_users() {
    let (_t, p) = tmp();
    if rustix::process::geteuid().is_root() {
        return; // root reads mode-000 directories
    }
    let wt = p.join("wt");
    fs::create_dir_all(&wt).unwrap();
    let proc_root = p.join("proc");
    fs::create_dir_all(&proc_root).unwrap();
    let hidden = fake_proc(&proc_root, 40, Some(&p), &[]);
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
    let mine = rustix::process::geteuid().as_raw();
    assert_eq!(scan_proc(&proc_root, &wt, mine), (Tri::Unknown, None));
    assert_eq!(
        scan_proc(&proc_root, &wt, mine + 1),
        (Tri::NotRunning, None)
    );
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn lsof_scan_should_read_the_pid_from_stdout_and_treat_exit_1_as_nothing_found() {
    let wt = "/Users/u/code/r/.claude/worktrees/agent-a41";
    let argv = ["lsof", "-F", "n", "+D", wt];

    let holder = FakeCommandRunner::new();
    holder.on(
        &argv,
        Ok(CmdOutput {
            stdout: format!("p4242\nn{wt}\nn{wt}/src\n").into_bytes(),
            stderr: Vec::new(),
        }),
    );
    assert_eq!(
        lsof_scan(&holder, Path::new(wt)),
        (Tri::Running, Some(("a process".into(), 4242)))
    );

    let none = FakeCommandRunner::new();
    none.on(
        &argv,
        Err(DeleteError::CommandFailed {
            status: 1,
            message: "lsof: WARNING: can't stat() overlay file system /var/lib/docker".into(),
        }),
    );
    assert_eq!(lsof_scan(&none, Path::new(wt)), (Tri::NotRunning, None));
}

#[test]
fn lsof_scan_should_be_unknown_when_lsof_is_missing_or_fails_otherwise() {
    let wt = Path::new("/Users/u/code/r/.claude/worktrees/agent-a41");
    let missing = FakeCommandRunner::new();
    missing.absent("lsof");
    assert_eq!(lsof_scan(&missing, wt), (Tri::Unknown, None));

    let broken = FakeCommandRunner::new();
    broken.on(
        &["lsof", "-F", "n", "+D", &wt.to_string_lossy()],
        Err(DeleteError::CommandFailed {
            status: 2,
            message: "boom".into(),
        }),
    );
    assert_eq!(lsof_scan(&broken, wt), (Tri::Unknown, None));

    let timeout = FakeCommandRunner::new();
    timeout.on(
        &["lsof", "-F", "n", "+D", &wt.to_string_lossy()],
        Err(DeleteError::Timeout),
    );
    assert_eq!(lsof_scan(&timeout, wt), (Tri::Unknown, None));
}

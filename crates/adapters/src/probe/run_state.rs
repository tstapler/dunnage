//! `SystemProbe::run_state` (Story 3.2.2): is a build directory in use by a live process? Linux scans
//! `/proc/<pid>/{cwd,fd,maps}` and try-locks `.cargo-lock`; macOS asks `lsof` through the `CommandRunner`.
//! Everything is read-only and fails closed: anything that stops the scan from being complete is `Unknown`.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use dunnage_core::ports::{Cmd, CommandRunner, RunState};
use dunnage_core::DeleteError;
use rustix::fs::{flock, FlockOperation};

#[cfg(not(target_os = "linux"))]
use crate::command::RealCommandRunner;

#[cfg(target_os = "linux")]
pub(super) fn run_state(path: &Path) -> RunState {
    scan_proc(
        Path::new("/proc"),
        path,
        rustix::process::geteuid().as_raw(),
    )
}

#[cfg(not(target_os = "linux"))]
pub(super) fn run_state(path: &Path) -> RunState {
    lsof_state(&RealCommandRunner::new(), path)
}

/// The names a holder may use for `dir`: as given and fully resolved (`/proc/<pid>/cwd` is canonical).
fn spellings(dir: &Path) -> Vec<PathBuf> {
    let mut v = vec![dir.to_path_buf()];
    if let Ok(c) = fs::canonicalize(dir) {
        if c != v[0] {
            v.push(c);
        }
    }
    v
}

fn under(p: &Path, dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|d| p.starts_with(d))
}

/// Whether another open file description holds a `flock` on `<dir>/.cargo-lock`. The lock is taken only for
/// the instant of the test and dropped with the descriptor; an absent file is not held.
pub fn cargo_lock_held(dir: &Path) -> RunState {
    let file = match fs::File::open(dir.join(".cargo-lock")) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return RunState::Idle,
        Err(_) => return RunState::Unknown,
    };
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => RunState::Idle,
        Err(rustix::io::Errno::WOULDBLOCK) => RunState::Running,
        Err(_) => RunState::Unknown,
    }
}

/// Scans `proc_root` (`/proc`) for any process whose cwd, open file or mapped file lies under `dir`, then
/// try-locks `.cargo-lock`. `Running` wins over `Unknown`. Processes of other users are skipped unless we are
/// root (their links cannot be read). A process of ours that denies access (`EACCES`/`EPERM`: non-dumpable or
/// capability-holding daemons such as `gpg-agent`) cannot be looked into and is skipped; any other read error,
/// an unreadable `proc_root`, or a scan that could look into no process at all is `Unknown`.
pub fn scan_proc(proc_root: &Path, dir: &Path, euid: u32) -> RunState {
    let Ok(entries) = fs::read_dir(proc_root) else {
        return RunState::Unknown;
    };
    let dirs = spellings(dir);
    let mut unknown = false;
    let mut inspected = 0usize;
    for e in entries.flatten() {
        if !e
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            continue;
        }
        let pdir = e.path();
        match fs::metadata(&pdir) {
            Ok(m) if euid != 0 && m.uid() != euid => continue,
            Ok(_) => {}
            Err(_) => continue, // exited mid-scan
        }
        match process_state(&pdir, &dirs) {
            Some(RunState::Running) => return RunState::Running,
            Some(RunState::Unknown) => unknown = true,
            Some(RunState::Idle) => inspected += 1,
            None => {}
        }
    }
    let lock = cargo_lock_held(dir);
    if lock == RunState::Running {
        return lock;
    }
    if inspected == 0 || unknown {
        return RunState::Unknown;
    }
    lock
}

/// `None` when the kernel denies every look into the process.
fn process_state(pdir: &Path, dirs: &[PathBuf]) -> Option<RunState> {
    let mut unknown = false;
    let mut denied = 0;
    match fs::read_link(pdir.join("cwd")) {
        Ok(p) if under(&p, dirs) => return Some(RunState::Running),
        Ok(_) => {}
        Err(e) => note(&e, &mut unknown, &mut denied),
    }
    match fs::read_dir(pdir.join("fd")) {
        Ok(fds) => {
            for fd in fds.flatten() {
                if let Ok(p) = fs::read_link(fd.path()) {
                    if under(&p, dirs) {
                        return Some(RunState::Running);
                    }
                }
            }
        }
        Err(e) => note(&e, &mut unknown, &mut denied),
    }
    match fs::read_to_string(pdir.join("maps")) {
        Ok(maps) => {
            // "start-end perms offset dev inode   path"; the path may hold spaces and ends " (deleted)"
            for line in maps.lines() {
                let Some(path) = line.splitn(6, ' ').nth(5).map(str::trim_start) else {
                    continue;
                };
                if under(Path::new(path.trim_end_matches(" (deleted)")), dirs) {
                    return Some(RunState::Running);
                }
            }
        }
        Err(e) => note(&e, &mut unknown, &mut denied),
    }
    if unknown {
        Some(RunState::Unknown)
    } else if denied == 3 {
        None
    } else {
        Some(RunState::Idle)
    }
}

fn note(e: &io::Error, unknown: &mut bool, denied: &mut u8) {
    match e.raw_os_error() {
        Some(libc::EACCES | libc::EPERM) => *denied += 1,
        Some(libc::ENOENT | libc::ESRCH) => {}
        _ => *unknown = true,
    }
}

/// macOS (and any host without `/proc`): `lsof -F n +D <dir>`. Output naming a file under `dir` is `Running`;
/// exit status 1 with nothing matching is `Idle`; a missing or failing `lsof` is `Unknown`.
pub fn lsof_state(runner: &dyn CommandRunner, dir: &Path) -> RunState {
    if !runner.exists("lsof") {
        return RunState::Unknown;
    }
    let args: Vec<OsString> = vec!["-F".into(), "n".into(), "+D".into(), dir.into()];
    match runner.run(&Cmd::new("lsof", args, false)) {
        Ok(out) => {
            let dirs = spellings(dir);
            let held = out
                .stdout_lossy()
                .lines()
                .filter_map(|l| l.strip_prefix('n'))
                .any(|p| under(Path::new(p), &dirs));
            if held {
                RunState::Running
            } else {
                RunState::Idle
            }
        }
        // lsof exits 1 when nothing matches (stderr may carry only warnings)
        Err(DeleteError::CommandFailed { status: 1, .. }) => RunState::Idle,
        Err(_) => RunState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, non_snake_case)]
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;

    use dunnage_core::ports::CmdOutput;
    use dunnage_core::testing::FakeCommandRunner;

    use super::*;

    #[cfg(target_os = "linux")]
    fn me() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    /// A harmless fixture process; killed and reaped on drop.
    struct Sleeper(Child);

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn sleeper(cwd: &Path, sh: &str) -> Sleeper {
        let c = Command::new("sh")
            .arg("-c")
            .arg(sh)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Sleeper(c)
    }

    fn wait_for(mut f: impl FnMut() -> bool) -> bool {
        for _ in 0..1500 {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[cfg(target_os = "linux")] // reads /proc
    #[test]
    fn idle_should_be_reported_When_no_process_uses_the_directory() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("app/target");
        fs::create_dir_all(&d).unwrap();
        assert_eq!(scan_proc(Path::new("/proc"), &d, me()), RunState::Idle);
    }

    #[cfg(target_os = "linux")] // reads /proc
    #[test]
    fn running_should_be_reported_When_a_child_has_its_cwd_inside() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("app/target");
        let sub = d.join("debug");
        fs::create_dir_all(&sub).unwrap();
        // spawned with the cwd already set, so there is nothing to wait for
        let _c = sleeper(&sub, "exec sleep 30");
        assert_eq!(scan_proc(Path::new("/proc"), &d, me()), RunState::Running);
        assert_eq!(run_state(&d), RunState::Running);
    }

    #[cfg(target_os = "linux")] // reads /proc
    #[test]
    fn running_should_be_reported_When_a_child_holds_a_file_open_inside() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("app/node_modules");
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("x"), "x").unwrap();
        let _c = sleeper(
            t.path(),
            &format!("exec 3<'{}'; exec sleep 30", d.join("x").display()),
        );
        assert!(wait_for(
            || scan_proc(Path::new("/proc"), &d, me()) == RunState::Running
        ));
    }

    /// Polls `f` for up to 5 s. A `flock` belongs to the open file description, so a sibling test thread that
    /// forks while a descriptor of this test is open keeps a copy of it until its `exec` and the lock reads as
    /// held (or, after the release, as still held) for that moment; the state a test asserts is the one after
    /// the copy is gone.
    fn eventually(mut f: impl FnMut() -> bool) -> bool {
        for _ in 0..500 {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn running_should_be_reported_When_a_file_inside_is_flocked() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("app/target");
        fs::create_dir_all(&d).unwrap();
        let lock = fs::File::create(d.join(".cargo-lock")).unwrap();
        assert!(
            eventually(|| cargo_lock_held(&d) == RunState::Idle),
            "an unlocked file reads as idle"
        );
        // The probe above held a lock on its own descriptor for an instant; a forked sibling may still hold a
        // copy of that descriptor, so the exclusive lock can say WouldBlock for a moment.
        assert!(
            eventually(|| flock(&lock, FlockOperation::NonBlockingLockExclusive).is_ok()),
            "the exclusive lock could not be taken"
        );
        assert_eq!(cargo_lock_held(&d), RunState::Running);
        #[cfg(target_os = "linux")]
        assert_eq!(scan_proc(Path::new("/proc"), &d, me()), RunState::Running);
        drop(lock);
        assert!(
            eventually(|| cargo_lock_held(&d) == RunState::Idle),
            "the lock is released with the descriptor"
        );
    }

    #[test]
    fn running_should_be_reported_When_a_child_process_holds_the_flock() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("app/target");
        fs::create_dir_all(&d).unwrap();
        let lock = d.join(".cargo-lock");
        fs::write(&lock, "").unwrap();
        // util-linux flock(1) takes the lock on fd 3 and the shell then keeps the descriptor open
        let have_flock = Command::new("flock")
            .arg("--version")
            .stdout(Stdio::null())
            .status()
            .is_ok();
        if !have_flock {
            return;
        }
        let _c = sleeper(
            t.path(),
            &format!("exec 3>'{}'; flock -x 3; exec sleep 30", lock.display()),
        );
        assert!(wait_for(|| cargo_lock_held(&d) == RunState::Running));
    }

    #[test]
    fn unknown_should_be_reported_When_proc_cannot_be_read() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(
            scan_proc(&t.path().join("no-proc"), t.path(), me()),
            RunState::Unknown
        );
        // a proc root with no numeric entries has not shown us any process either
        assert_eq!(scan_proc(t.path(), t.path(), me()), RunState::Unknown);
    }

    #[test]
    fn lsof_should_report_running_When_output_names_a_file_under_the_directory() {
        let r = FakeCommandRunner::new();
        r.on(
            &["lsof", "-F", "n", "+D", "/Users/u/app/node_modules"],
            Ok(CmdOutput {
                stdout: b"p123\nn/Users/u/app/node_modules/.bin/vite\n".to_vec(),
                stderr: Vec::new(),
            }),
        );
        assert_eq!(
            lsof_state(&r, Path::new("/Users/u/app/node_modules")),
            RunState::Running
        );
    }

    #[test]
    fn lsof_should_report_idle_When_it_exits_1_and_unknown_When_missing_or_broken() {
        let d = Path::new("/Users/u/app/target");
        let argv = ["lsof", "-F", "n", "+D", "/Users/u/app/target"];
        let none = FakeCommandRunner::new();
        none.on(
            &argv,
            Err(DeleteError::CommandFailed {
                status: 1,
                message: String::new(),
            }),
        );
        assert_eq!(lsof_state(&none, d), RunState::Idle);

        let missing = FakeCommandRunner::new();
        missing.absent("lsof");
        assert_eq!(lsof_state(&missing, d), RunState::Unknown);

        let broken = FakeCommandRunner::new();
        broken.on(
            &argv,
            Err(DeleteError::CommandFailed {
                status: 2,
                message: "boom".into(),
            }),
        );
        assert_eq!(lsof_state(&broken, d), RunState::Unknown);
    }

    #[test]
    fn cargo_lock_should_be_idle_When_the_file_is_absent() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(cargo_lock_held(t.path()), RunState::Idle);
    }
}

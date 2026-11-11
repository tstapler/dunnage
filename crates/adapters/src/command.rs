//! `RealCommandRunner`: argv, no shell, a deadline and captured output (Task 1.1.2c). Every entry point
//! asks `core::ports::classify` first; `git worktree remove` has one door, [`RealCommandRunner::remove_worktree`].

use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::signals::ChildInFlight;
use crate::state::shell_quote_path;
use disk_cleanup_core::ports::{
    classify, worktree_remove_argv, Class, Cmd, CmdOutput, CommandRunner,
};
use disk_cleanup_core::{first_line_capped, ApplyToken, DeleteError, WorktreeRemoval};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(10);
const EIO: i32 = 5;

#[derive(Debug, Clone)]
pub struct RealCommandRunner {
    timeout: Duration,
}

impl Default for RealCommandRunner {
    fn default() -> RealCommandRunner {
        RealCommandRunner {
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl RealCommandRunner {
    pub fn new() -> RealCommandRunner {
        RealCommandRunner::default()
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    #[must_use]
    pub fn with_timeout(timeout: Duration) -> RealCommandRunner {
        RealCommandRunner { timeout }
    }
}

fn drain<R: Read + Send + 'static>(r: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = r {
            let _ = r.read_to_end(&mut buf);
        }
        buf
    })
}

/// Spawns `program args` directly (no shell, stdin closed) and waits at most `timeout`; on overrun the
/// process is killed and reaped. Exit 0 is `Ok`; any other exit is `CommandFailed` carrying the first line
/// of stderr (stdout when stderr is empty).
pub(crate) fn spawn_with_deadline(
    program: &OsString,
    args: &[OsString],
    timeout: Duration,
) -> Result<CmdOutput, DeleteError> {
    spawn_inner(program, args, timeout, None)
}

/// As [`spawn_with_deadline`]. The child runs in its own process group, so the terminal's Ctrl-C never
/// reaches it; with `in_flight` set, a second SIGINT is acknowledged and ignored until it ends (Story 2.2.2).
fn spawn_inner(
    program: &OsString,
    args: &[OsString],
    timeout: Duration,
    in_flight: Option<String>,
) -> Result<CmdOutput, DeleteError> {
    let mut child = Command::new(program)
        .args(args)
        .process_group(0)
        // `git status` otherwise takes and drops index.lock, which moves the admin entry's mtime that the
        // worktree age floor reads (Story 3.1.2); harmless to every other program
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| DeleteError::Io(e.raw_os_error().unwrap_or(EIO)))?;
    let _in_flight = in_flight.map(ChildInFlight::begin);
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out.join();
                let _ = err.join();
                return Err(DeleteError::Timeout);
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => return Err(DeleteError::Io(e.raw_os_error().unwrap_or(EIO))),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if status.success() {
        return Ok(CmdOutput { stdout, stderr });
    }
    let text = if stderr.iter().any(|b| !b.is_ascii_whitespace()) {
        &stderr
    } else {
        &stdout
    };
    Err(DeleteError::CommandFailed {
        status: status.code().unwrap_or(-1),
        message: first_line_capped(&String::from_utf8_lossy(text)),
    })
}

fn on_path(program: &str) -> bool {
    if program.is_empty() || program.contains('/') {
        return false;
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let p = dir.join(program);
        Path::new(&p)
            .metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

impl CommandRunner for RealCommandRunner {
    fn exists(&self, program: &str) -> bool {
        on_path(program)
    }

    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, DeleteError> {
        if classify(cmd) != Some(Class::ReadOnly) {
            return Err(DeleteError::Refused);
        }
        spawn_with_deadline(&cmd.program, &cmd.args, self.timeout)
    }

    fn run_mutating(&self, cmd: &Cmd, _proof: &ApplyToken) -> Result<CmdOutput, DeleteError> {
        if classify(cmd) != Some(Class::Mutating) {
            return Err(DeleteError::Refused);
        }
        spawn_with_deadline(&cmd.program, &cmd.args, self.timeout)
    }

    /// The one place that builds `git -C <main_repo> worktree remove <path>`; it is not an allowlist row, so
    /// it never goes through `run_mutating`.
    fn remove_worktree(
        &self,
        w: &WorktreeRemoval,
        _proof: &ApplyToken,
    ) -> Result<CmdOutput, DeleteError> {
        let desc = format!("git worktree remove {}", shell_quote_path(w.path()));
        spawn_inner(
            &OsString::from("git"),
            &worktree_remove_argv(w),
            self.timeout,
            Some(desc),
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn spawn_should_return_timeout_and_kill_the_child_When_the_deadline_passes() {
        let started = Instant::now();
        let r = spawn_with_deadline(&"sleep".into(), &os(&["30"]), Duration::from_millis(100));
        assert_eq!(r, Err(DeleteError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn spawn_should_report_command_failed_with_the_first_stderr_line_When_exit_is_non_zero() {
        let r = spawn_with_deadline(
            &"sh".into(),
            &os(&["-c", "echo first >&2; echo second >&2; exit 3"]),
            Duration::from_secs(10),
        );
        assert_eq!(
            r,
            Err(DeleteError::CommandFailed {
                status: 3,
                message: "first".into()
            })
        );
    }

    #[test]
    fn spawn_should_fall_back_to_stdout_When_stderr_is_empty() {
        let r = spawn_with_deadline(
            &"sh".into(),
            &os(&["-c", "echo out; exit 1"]),
            Duration::from_secs(10),
        );
        assert_eq!(
            r,
            Err(DeleteError::CommandFailed {
                status: 1,
                message: "out".into()
            })
        );
    }

    #[test]
    fn spawn_should_capture_stdout_When_exit_is_zero() {
        let r = spawn_with_deadline(
            &"sh".into(),
            &os(&["-c", "printf hi"]),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(r.stdout_lossy(), "hi");
    }

    #[test]
    fn spawn_should_report_io_enoent_When_the_program_does_not_exist() {
        let r = spawn_with_deadline(&"dc-no-such-program".into(), &[], Duration::from_secs(1));
        assert_eq!(r, Err(DeleteError::Io(2)));
    }

    #[test]
    fn exists_should_find_sh_and_not_a_path_or_a_missing_program() {
        let r = RealCommandRunner::new();
        assert!(r.exists("sh"));
        assert!(!r.exists("/bin/sh"));
        assert!(!r.exists("dc-no-such-program"));
        assert!(!r.exists(""));
    }
}

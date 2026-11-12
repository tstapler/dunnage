//! Is a directory in use (Story 3.1.2, triad U-1)? Any process whose working directory is the path or below
//! it, or that holds an open file below it, counts. Read-only.
//!
//! Linux reads `/proc/<pid>/cwd` and `/proc/<pid>/fd/*`. macOS has no `/proc`: `lsof -F n +D <path>` runs
//! through the `CommandRunner`, and a missing or failing `lsof` is `Unknown`, never "not in use".
//!
//! `Running` means a holder was found, `NotRunning` that every process this user owns was looked at and none
//! holds the path, `Unknown` that something that could hide a holder could not be read.

use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use dunnage_core::ports::{Cmd, CommandRunner};
use dunnage_core::{DeleteError, Tri};

/// `(answer, first holder as (process name, pid))`.
pub type InUse = (Tri, Option<(String, u32)>);

pub(super) fn cwd_in_use(path: &Path) -> InUse {
    if cfg!(target_os = "linux") {
        scan_proc(Path::new("/proc"), path, super::basic::euid())
    } else {
        lsof_scan(&crate::RealCommandRunner::new(), path)
    }
}

fn is_gone(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ESRCH))
}

/// The process name from `/proc/<pid>/comm`.
fn comm(pid_dir: &Path) -> String {
    std::fs::read_to_string(pid_dir.join("comm"))
        .map(|s| s.trim_end().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown process".to_string())
}

/// Scans a `/proc`-shaped tree at `proc_root` for holders of `target`. `euid` decides which unreadable
/// processes matter: another user's process cannot be read and is not ours to hide a holder, an
/// unreadable process of our own user can.
pub fn scan_proc(proc_root: &Path, target: &Path, euid: u32) -> InUse {
    let target: PathBuf = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    let Ok(rd) = std::fs::read_dir(proc_root) else {
        return (Tri::Unknown, None);
    };
    let mut unknown = false;
    for entry in rd.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let dir = entry.path();
        match holds(&dir, &target) {
            Held::Yes => return (Tri::Running, Some((comm(&dir), pid))),
            Held::No => {}
            Held::Unreadable => {
                let ours = std::fs::metadata(&dir).map_or(true, |m| m.uid() == euid);
                unknown |= ours;
            }
        }
    }
    (
        if unknown {
            Tri::Unknown
        } else {
            Tri::NotRunning
        },
        None,
    )
}

enum Held {
    Yes,
    No,
    Unreadable,
}

fn holds(pid_dir: &Path, target: &Path) -> Held {
    let mut unreadable = false;
    match std::fs::read_link(pid_dir.join("cwd")) {
        Ok(l) if l.starts_with(target) => return Held::Yes,
        Ok(_) => {}
        Err(e) if is_gone(&e) => return Held::No,
        Err(_) => unreadable = true,
    }
    match std::fs::read_dir(pid_dir.join("fd")) {
        Ok(fds) => {
            for fd in fds.flatten() {
                match std::fs::read_link(fd.path()) {
                    Ok(l) if l.starts_with(target) => return Held::Yes,
                    Ok(_) => {}
                    Err(e) if is_gone(&e) => {}
                    Err(_) => unreadable = true,
                }
            }
        }
        Err(e) if is_gone(&e) => return Held::No,
        Err(_) => unreadable = true,
    }
    if unreadable {
        Held::Unreadable
    } else {
        Held::No
    }
}

/// macOS: `lsof -F n +D <path>`. A holder gives `p<pid>` and `n<path>` lines on stdout; with none, stdout is
/// empty and the status is 1 (the runner reports that as `CommandFailed { status: 1, .. }`). Warnings on
/// stderr are ignored. The name is not in that output, so the holder is reported as `a process`. Any other
/// failure, and a missing `lsof`, is `Unknown`.
pub fn lsof_scan(runner: &dyn CommandRunner, path: &Path) -> InUse {
    if !runner.exists("lsof") {
        return (Tri::Unknown, None);
    }
    let argv: [OsString; 4] = [
        "-F".into(),
        "n".into(),
        "+D".into(),
        path.as_os_str().to_owned(),
    ];
    match runner.run(&Cmd::new("lsof", argv, false)) {
        Ok(out) => match first_pid(&out.stdout_lossy()) {
            Some(pid) => (Tri::Running, Some(("a process".to_string(), pid))),
            None => (Tri::NotRunning, None),
        },
        Err(DeleteError::CommandFailed { status: 1, .. }) => (Tri::NotRunning, None),
        Err(_) => (Tri::Unknown, None),
    }
}

fn first_pid(stdout: &str) -> Option<u32> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix('p').and_then(|n| n.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_pid_should_read_the_first_p_line() {
        assert_eq!(first_pid("p123\nn/x\np9\nn/y\n"), Some(123));
        assert_eq!(first_pid(""), None);
        assert_eq!(first_pid("n/x\n"), None);
        assert_eq!(first_pid("pabc\n"), None);
    }
}

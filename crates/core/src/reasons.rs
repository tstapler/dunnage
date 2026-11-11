//! The reason, error and warning texts of `design/ux.md` 3.5 (the testable contract; the catalogue test in
//! `tests/reason_catalogue.rs` holds a copy of the table and compares).
//!
//! Markdown backticks around commands are not part of the terminal text. Placeholders such as `<path>` or
//! `N` stay in [`SkipReason::reason_text`] and [`SkipReason::next_action`]; the story that renders a row fills
//! them from its own facts.

use std::path::{Path, PathBuf};

use crate::delete_types::DeleteError;
use crate::outcome::SkipReason;

impl SkipReason {
    /// What happened and why.
    pub fn reason_text(self) -> &'static str {
        match self {
            SkipReason::Dirty => "dirty working tree",
            SkipReason::Locked => "worktree is locked",
            SkipReason::TooRecent => "younger than --min-age 14d (build dirs: the effective floor, never below 60m)",
            SkipReason::UnpushedCommits => "N unpushed commits",
            SkipReason::DetachedHead => "detached HEAD not on any ref",
            SkipReason::RunningBuild => "in use by a running process",
            SkipReason::RunStateUnknown => "cannot tell whether a build is running (/proc unreadable)",
            SkipReason::IdentityChanged => "changed since scan",
            SkipReason::Symlink => "out of scope: symlink",
            SkipReason::CrossDevice => "out of scope: cross-device",
            SkipReason::MountPoint => "out of scope: mount point",
            SkipReason::OutsideRoot => "out of scope: outside root",
            SkipReason::ProtectedPath => "out of scope: protected path",
            SkipReason::VolumeReferenced => "volume used by container/compose project <x>",
            SkipReason::ImageReferenced => "image used by container <x>",
            SkipReason::DaemonUnavailable => "docker: daemon not reachable (<reason>)",
            SkipReason::LibraryUnmounted => "Steam library <path> not mounted",
            SkipReason::SteamRunning => "Steam is running or downloading",
            SkipReason::NeedsRoot => "needs root",
            SkipReason::ToolMissing => "<tool> not installed",
            SkipReason::AlreadyRunning => "btrfs balance already running",
            SkipReason::Gone => "already gone",
            SkipReason::Aborted => "aborted: ENOSPC / interrupted",
            SkipReason::KeptAsNewest => "kept as newest of N duplicates",
            SkipReason::KindNotSelected => "not selected",
            SkipReason::RootRestricted => "running as root: your user caches are skipped",
            SkipReason::NonLocalDockerContext => "non-local docker context '<name>'",
            SkipReason::UnsupportedOnThisOS => "not supported on <OS>",
            SkipReason::UnrecognisedOutput => "unrecognised <tool> output; nothing listed",
            SkipReason::DockerVersionUntested => "docker <version> is outside the tested range; list-only",
            SkipReason::LooksLikeDatabase => "volume looks like a database (<signature>, or larger than 1 GiB)",
            SkipReason::ContentsUnknown => "volume contents not readable, cannot rule out a database",
            SkipReason::LocalOnlyImage => "image built on this machine (no registry digest): pulling cannot bring it back",
            SkipReason::InUse => "in use: <process> (pid <N>) has its working directory or an open file under this worktree",
            SkipReason::HasIgnoredFiles => "holds ignored files that git would delete with it (<N> files, <size>, for example .env, *.sqlite)",
            SkipReason::NotReproducible => "not reproducible: <detail> (an npm-linked package points outside the project, or no lockfile next to node_modules)",
            SkipReason::MayHoldSaves => "prefix holds files in <dir>, possibly save games",
            SkipReason::OptInRequired => "list-only: needs its own flag",
        }
    }

    /// What to do next; `none...` when there is nothing to do (the catalogue test requires one or the other).
    pub fn next_action(self) -> &'static str {
        match self {
            SkipReason::Dirty => "commit, stash or discard in that worktree",
            SkipReason::Locked => "git worktree unlock <path> if intended",
            SkipReason::TooRecent => "lower with --min-age (build dirs cannot go below 60m)",
            SkipReason::UnpushedCommits => "push or delete the branch yourself",
            SkipReason::DetachedHead => "create a branch first",
            SkipReason::RunningBuild => "stop the process, re-run",
            SkipReason::RunStateUnknown => "run as the owning user",
            SkipReason::IdentityChanged => "re-run the plan",
            SkipReason::Symlink | SkipReason::CrossDevice | SkipReason::MountPoint | SkipReason::OutsideRoot | SkipReason::ProtectedPath => "none (never touched)",
            SkipReason::VolumeReferenced | SkipReason::ImageReferenced => "none",
            SkipReason::DaemonUnavailable => "start Docker yourself; the tool never starts it",
            SkipReason::LibraryUnmounted => "mount it, re-run",
            SkipReason::SteamRunning => "quit Steam, re-run",
            SkipReason::NeedsRoot => "sudo <absolute path>/disk-cleanup --apply --kinds <exact-id> (see C5 and 1.5)",
            SkipReason::ToolMissing => "none",
            SkipReason::AlreadyRunning => "btrfs balance status /",
            SkipReason::Gone => "none",
            SkipReason::Aborted => "free a little space (the summary prints the hint), then dry run and apply again",
            SkipReason::KeptAsNewest => "none: the newest copy is kept on purpose (after a partial scan: --rescan)",
            SkipReason::KindNotSelected => "add the kind to --kinds",
            SkipReason::RootRestricted => "run as your user; use sudo only for cache-apt (the decision is euid 0, whatever HOME holds: sudoers decides whether sudo sets HOME to /root, and that file is root-only, so the tool does not assume it; section 1.5)",
            SkipReason::NonLocalDockerContext => "docker context use default",
            SkipReason::UnsupportedOnThisOS => "none",
            SkipReason::UnrecognisedOutput => "update disk-cleanup or report the output shape (the tool never guesses)",
            SkipReason::DockerVersionUntested => "none (removal needs a tested Docker version)",
            SkipReason::LooksLikeDatabase => "name it with --volume <name> if you are sure",
            SkipReason::ContentsUnknown => "--volume <name>, or dry run as root: sudo <absolute path>/disk-cleanup --kinds docker-volumes (its plan-id is root's, so apply as root too; section 1.5)",
            SkipReason::LocalOnlyImage => "docker rmi <name> by hand if you are sure",
            SkipReason::InUse => "close that session or wait; the tool never stops a process",
            SkipReason::HasIgnoredFiles => "back them up or remove them yourself, or --worktree-ignored discard if they are disposable",
            SkipReason::NotReproducible => "remove it yourself if you are sure",
            SkipReason::MayHoldSaves => "--steam-saves discard if intended",
            SkipReason::OptInRequired => "add --docker-volumes anonymous or --docker-volumes named",
        }
    }

    /// `RISK` only for [`SkipReason::KindNotSelected`]; every other skip shows as `SKIP`.
    pub fn status_word(self) -> &'static str {
        if self == SkipReason::KindNotSelected {
            "RISK"
        } else {
            "SKIP"
        }
    }
}

/// Short errno phrase for the run-level texts; unknown errnos fall back to the generic form.
pub fn errno_phrase(errno: i32) -> String {
    match errno {
        13 | 1 => "permission denied".into(),
        28 | 122 => "no space left on device".into(),
        2 => "no such file or directory".into(),
        30 => "read-only file system".into(),
        n => format!("I/O error (errno {n})"),
    }
}

impl DeleteError {
    /// The error text of an `ERR` row and the audit `result` line (`design/ux.md` 3.5, error texts).
    /// Scope errors are `SKIP` rows; see [`DeleteError::skip_reason`].
    pub fn text(&self) -> String {
        match self {
            DeleteError::CommandFailed { status, message } if message.is_empty() => {
                format!("command exited {status}")
            }
            DeleteError::CommandFailed { status, message } => {
                format!("command exited {status}: {message}")
            }
            DeleteError::Timeout => "command timed out".into(),
            DeleteError::Refused => "command refused".into(),
            DeleteError::Io(13) => "permission denied (EACCES)".into(),
            DeleteError::Io(28) | DeleteError::NoSpace => "no space left on device".into(),
            DeleteError::Io(n) => format!("I/O error (errno {n})"),
            DeleteError::Busy => "directory changed while deleting (not empty)".into(),
            DeleteError::IdentityMismatch
            | DeleteError::Symlink
            | DeleteError::ProtectedPath
            | DeleteError::OutsideScope
            | DeleteError::CrossDevice => self
                .skip_reason()
                .map_or_else(String::new, |r| r.reason_text().to_string()),
        }
    }

    /// The `SKIP` row a scope error is shown as; `None` for errors that are `ERR` rows.
    pub fn skip_reason(&self) -> Option<SkipReason> {
        match self {
            DeleteError::IdentityMismatch => Some(SkipReason::IdentityChanged),
            DeleteError::Symlink => Some(SkipReason::Symlink),
            DeleteError::ProtectedPath => Some(SkipReason::ProtectedPath),
            DeleteError::OutsideScope => Some(SkipReason::OutsideRoot),
            DeleteError::CrossDevice => Some(SkipReason::CrossDevice),
            _ => None,
        }
    }
}

/// A run-level failure that ends the run. Each text ends with its next action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FatalError {
    LockHeld {
        pid: u32,
    },
    AuditUnwritable {
        path: PathBuf,
        errno: i32,
    },
    RootsUnreadable {
        path: PathBuf,
        errno: i32,
    },
    Starved {
        unallocated_mib: u64,
        metadata_headroom_mib: u64,
    },
    StateUnreadable {
        path: PathBuf,
        reason: String,
    },
}

/// `'...'` unless the text is made of characters a shell never splits or expands.
fn sh_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

impl FatalError {
    /// The text printed on stderr.
    pub fn text(&self) -> String {
        match self {
            FatalError::LockHeld { pid } => format!(
                "error: another disk-cleanup run is active (pid {pid}). Nothing was deleted. Wait for it to finish ('pgrep -a disk-cleanup' shows it), then run again."
            ),
            FatalError::AuditUnwritable { path, errno: 28 | 122 } => format!(
                "error: cannot write the audit log {} (no space left on device). Nothing was deleted. The log shares the full filesystem: free about 1 MiB by hand (empty the trash, remove one disposable file), then run again.",
                path.display()
            ),
            FatalError::AuditUnwritable { path, errno } => {
                let dir = path.parent().unwrap_or_else(|| Path::new("."));
                format!(
                    "error: cannot write the audit log {} ({}). Nothing was deleted. Fix the ownership or mode of {}, or point XDG_STATE_HOME at a writable directory, then run again.",
                    path.display(),
                    errno_phrase(*errno),
                    dir.display()
                )
            }
            FatalError::RootsUnreadable { path, errno } => format!(
                "error: root {} does not exist or is unreadable ({}). Nothing scanned. Check the spelling of --root (it must be a directory you can read) or drop --root to use the default roots.",
                path.display(),
                errno_phrase(*errno)
            ),
            FatalError::Starved {
                unallocated_mib,
                metadata_headroom_mib,
            } => format!(
                "error: btrfs unallocated {unallocated_mib} MiB and metadata headroom {metadata_headroom_mib} MiB: deleting could fail with ENOSPC. Nothing was deleted. Free ~1 GiB by hand (empty trash, remove one known-disposable file), then re-run."
            ),
            FatalError::StateUnreadable { path, reason } => format!(
                "error: cannot read filesystem state for {}: {reason}. Check that the path is mounted ('df -- {}') or run as a user that can read /sys/fs/btrfs.",
                path.display(),
                sh_quote(path)
            ),
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            FatalError::LockHeld { .. }
            | FatalError::AuditUnwritable { .. }
            | FatalError::RootsUnreadable { .. } => 2,
            FatalError::Starved { .. } => 3,
            FatalError::StateUnreadable { .. } => 74,
        }
    }
}

impl std::fmt::Display for FatalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text())
    }
}

impl std::error::Error for FatalError {}

/// Warnings and notes of `design/ux.md` 3.5; each says what happened, why and the next action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    PreviousRunUnfinished { run_id: String, audit_path: PathBuf },
    PlanNotSaved { errno: i32, path: PathBuf },
    LastPlanUnreadable { path: PathBuf },
    PlanDuringApply { pid: u32 },
    ReservesCreated { state_dir: PathBuf },
    PlanLockBusy,
}

impl Warning {
    pub fn text(&self) -> String {
        match self {
            Warning::PreviousRunUnfinished { run_id, audit_path } => format!(
                "warning: previous run {run_id} did not finish (killed or power loss); {a} may show a partly removed item. Check it with 'disk-cleanup audit verify' or jq 'select(.run_id==\"{run_id}\")' {a}. A partly removed build dir: dry run again; a partly removed worktree: 'git -C <repo> worktree list', then 'git -C <repo> worktree prune' once its directory is gone.",
                a = audit_path.display()
            ),
            Warning::PlanNotSaved { errno, path } => format!(
                "warning: could not save the plan ({} on {}). Its plan-id cannot be used; see the NOT usable block.",
                errno_phrase(*errno),
                path.display()
            ),
            Warning::LastPlanUnreadable { path } => format!(
                "warning: last_plan ({}) is unreadable or written by a newer version; treating it as no dry run. Dry run again.",
                path.display()
            ),
            Warning::PlanDuringApply { pid } => format!(
                "note: another disk-cleanup apply is running (pid {pid}); sizes and items below are in flux and a plan-id from this report may be refused as changed once it finishes."
            ),
            Warning::ReservesCreated { state_dir } => format!(
                "note: created 2 reserve files of 1 MiB in {} (they let the tool save a plan and open its audit log when the disk is full)",
                state_dir.display()
            ),
            Warning::PlanLockBusy => {
                "warning: could not save the plan: last_plan.lock busy for 2 s. Dry run again in a moment.".into()
            }
        }
    }
}

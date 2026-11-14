//! Plain-data types of the delete-engine contract (Story 0.1.3). The engine itself lives in `adapters`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::allowed::ExplorerScope;
use crate::ids::{AllocatedBytes, ApparentBytes, BirthTime, Identity, RootDev};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatInfo {
    pub ident: Identity,
    pub kind: FileKind,
    pub mode: u32,
    pub nlink: u64,
    /// 512-byte units.
    pub blocks: u64,
    pub size: u64,
    pub mtime_secs: i64,
    pub mtime_nsec: u32,
    pub mnt_id: Option<u64>,
    /// `None` where the filesystem reports no birth time.
    pub btime: Option<BirthTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Dir,
    File,
    Symlink,
    Fifo,
    Socket,
    CharDev,
    BlockDev,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryInfo {
    pub name: OsString,
    pub kind: FileKind,
    pub ino: u64,
}

/// What a delete counted. On a failed delete it is what was counted before the failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Freed {
    pub attributed_bytes: AllocatedBytes,
    pub apparent_bytes: ApparentBytes,
    pub entries_removed: u64,
    pub inodes_freed: u64,
}

/// `DeleteError` and `DeleteFailure` are `Clone` but not `Copy` because `CommandFailed` carries a message:
///
/// ```compile_fail
/// use dunnage_core::DeleteError;
/// fn is_copy<T: Copy>() {}
/// is_copy::<DeleteError>();
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteError {
    IdentityMismatch,
    CrossDevice,
    Symlink,
    ProtectedPath,
    OutsideScope,
    Busy,
    NoSpace,
    /// Raw errno only.
    Io(i32),
    /// `message` is the first line of stderr, at most [`CMD_MSG_MAX`] bytes.
    CommandFailed {
        status: i32,
        message: String,
    },
    Refused,
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteFailure {
    pub error: DeleteError,
    /// What was counted before the failure.
    pub freed: Freed,
}

pub const CMD_MSG_MAX: usize = 200;

/// First non-empty line, control characters turned into spaces, cut on a char boundary at
/// [`CMD_MSG_MAX`] bytes and ended with `...` when cut.
pub fn first_line_capped(raw: &str) -> String {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let clean: String = line
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if clean.len() <= CMD_MSG_MAX {
        return clean;
    }
    let mut end = CMD_MSG_MAX - 3;
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &clean[..end])
}

/// Set once to stop a running delete; the engine checks it before every unlink.
#[derive(Debug, Default)]
pub struct AbortFlag(AtomicBool);

impl AbortFlag {
    pub fn new() -> AbortFlag {
        AbortFlag::default()
    }

    pub fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowedTarget {
    Exact(PathBuf),
    Within {
        root: PathBuf,
        name: NamePattern,
    },
    /// The explorer's marked path: strictly below a scan root, no `.git` component, never a root, `$HOME`
    /// or an ancestor, and exactly the path the witness was made for (ADR-006, review C1/C2).
    BelowScanRoot(ExplorerScope),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamePattern {
    NodeModules,
    CargoTarget,
    AgentWorktree,
    SteamAppId,
    SteamInstallDir,
    CargoTargetPart,
    CargoHomeEntry,
}

/// Everything rule F and the descent need; see `DeletePolicy::validate` in `floor_rule.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletePolicy {
    pub root_dev: RootDev,
    pub root_mnt: Option<u64>,
    pub home: PathBuf,
    pub home_id: Identity,
    pub scan_roots: Vec<PathBuf>,
    pub allowed: AllowedTarget,
    pub target: PathBuf,
    pub require_sibling_any: Vec<OsString>,
    pub max_depth: u32,
    pub restore_owner_write: bool,
}

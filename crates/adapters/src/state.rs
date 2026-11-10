//! The state directory (`$XDG_STATE_HOME/disk-cleanup`): the two reserve files, `last_plan` behind the
//! `PlanStore` port, the unclean-run notice's inputs and `outcome.jsonl` (Story 2.2.1, Tasks 2.2.1b, c, e).
//!
//! Nothing here reads a home directory: the caller resolves the path (`cli/src/wiring/dirs.rs`), and under
//! sudo passes the invoking user's home through an injected lookup (see [`sudo_state_dir`]).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use disk_cleanup_core::ports::merge_saved;
use disk_cleanup_core::ports::PlanStore;
use disk_cleanup_core::{DiskVerdict, SaveOutcome, SavedPlan, SavedSelection, Warning};
use rustix::fs::{flock, FlockOperation};
use serde::{Deserialize, Serialize};

pub const RESERVE_BYTES: u64 = 1 << 20;
/// A truncated reserve is re-created only with this much available, so a full disk is never asked for it.
pub const RESERVE_RECREATE_MIN_AVAIL: u64 = 16 << 20;
/// `SaveOutcome::NotSaved` errno for a `last_plan.lock` that stayed busy (`EWOULDBLOCK`).
pub const LOCK_BUSY_ERRNO: i32 = 11;
pub const LOCK_WAIT: Duration = Duration::from_secs(2);
/// `outcome.jsonl` is renamed to `outcome.jsonl.1` once it passes this size.
pub const OUTCOME_ROTATE_BYTES: u64 = 256 * 1024;
pub const LAST_PLAN_SCHEMA: u32 = 1;

const ENOSPC: i32 = 28;
const EDQUOT: i32 = 122;

pub fn is_no_space(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(ENOSPC | EDQUOT))
}

/// Under sudo (`euid` 0 and `SUDO_USER` set) the state dir is the invoking user's, not root's; the home comes
/// from the injected lookup, never from this crate.
pub fn sudo_state_dir(
    euid: u32,
    sudo_user: Option<&str>,
    default_state_home: &Path,
    passwd_home: &dyn Fn(&str) -> Option<PathBuf>,
) -> PathBuf {
    let base = match sudo_user.filter(|u| euid == 0 && !u.is_empty()) {
        Some(user) => passwd_home(user).map_or_else(
            || default_state_home.to_path_buf(),
            |h| h.join(".local/state"),
        ),
        None => default_state_home.to_path_buf(),
    };
    base.join("disk-cleanup")
}

/// The files of one state directory.
#[derive(Debug, Clone)]
pub struct StatePaths {
    pub dir: PathBuf,
}

impl StatePaths {
    pub fn audit(&self) -> PathBuf {
        self.dir.join("audit.jsonl")
    }
    pub fn audit_reserve(&self) -> PathBuf {
        self.dir.join("audit.reserve")
    }
    pub fn last_plan(&self) -> PathBuf {
        self.dir.join("last_plan")
    }
    pub fn last_plan_reserve(&self) -> PathBuf {
        self.dir.join("last_plan.reserve")
    }
    pub fn last_plan_lock(&self) -> PathBuf {
        self.dir.join("last_plan.lock")
    }
    pub fn outcome(&self) -> PathBuf {
        self.dir.join("outcome.jsonl")
    }
}

/// Owner for files this process creates: under sudo the state dir belongs to the invoking user, and a
/// root-owned file there would break the next unprivileged run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

impl Owner {
    /// Owner of the nearest existing ancestor of `dir`, only when `euid` is 0 and that ancestor is not root's.
    pub fn for_euid(euid: u32, dir: &Path) -> Option<Owner> {
        if euid != 0 {
            return None;
        }
        let m = dir.ancestors().find_map(|a| fs::metadata(a).ok())?;
        (m.uid() != 0).then(|| Owner {
            uid: m.uid(),
            gid: m.gid(),
        })
    }

    pub fn apply(self, path: &Path) -> io::Result<()> {
        std::os::unix::fs::chown(path, Some(self.uid), Some(self.gid))
    }
}

pub fn chown_if(owner: Option<Owner>, path: &Path) {
    if let Some(o) = owner {
        let _ = o.apply(path);
    }
}

/// What opening the state directory did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateInit {
    pub reserves_created: bool,
}

/// An open state directory. `open` creates the directory and, before any other state file, the two reserves.
#[derive(Debug, Clone)]
pub struct StateDir {
    pub paths: StatePaths,
    pub owner: Option<Owner>,
}

impl StateDir {
    /// `avail` reports the bytes available to the caller on the filesystem of the given path (`statvfs`).
    /// A missing reserve is created only when at least [`RESERVE_RECREATE_MIN_AVAIL`] is available.
    pub fn open(
        dir: &Path,
        euid: u32,
        avail: &dyn Fn(&Path) -> io::Result<u64>,
    ) -> io::Result<(StateDir, StateInit)> {
        let owner = Owner::for_euid(euid, dir);
        let existed = dir.is_dir();
        fs::create_dir_all(dir)?;
        if !existed {
            chown_if(owner, dir);
        }
        let sd = StateDir {
            paths: StatePaths {
                dir: dir.to_path_buf(),
            },
            owner,
        };
        let mut init = StateInit::default();
        for r in [sd.paths.audit_reserve(), sd.paths.last_plan_reserve()] {
            if !r.exists() && avail(dir).is_ok_and(|a| a >= RESERVE_RECREATE_MIN_AVAIL) {
                match create_reserve(&r) {
                    Ok(()) => {
                        chown_if(owner, &r);
                        init.reserves_created = true;
                    }
                    Err(_) => {
                        let _ = fs::remove_file(&r);
                    }
                }
            }
        }
        Ok((sd, init))
    }

    pub fn reserves_note(&self, init: &StateInit) -> Option<Warning> {
        init.reserves_created.then(|| Warning::ReservesCreated {
            state_dir: self.paths.dir.clone(),
        })
    }
}

/// 1 MiB of real zeros (not a hole), so btrfs allocates the space.
fn create_reserve(path: &Path) -> io::Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE)
        .open(path)?;
    let chunk = vec![0u8; 64 * 1024];
    for _ in 0..(RESERVE_BYTES / chunk.len() as u64) {
        f.write_all(&chunk)?;
    }
    f.sync_data()
}

/// Gives the space of a reserve back (truncate to zero). `Ok(false)` when there is nothing to release.
pub fn release_reserve(path: &Path) -> io::Result<bool> {
    let f = match OpenOptions::new().write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if f.metadata()?.len() == 0 {
        return Ok(false);
    }
    f.set_len(0)?;
    f.sync_all()?;
    Ok(true)
}

// ---------------------------------------------------------------------------------------------------------
// last_plan

#[derive(Serialize, Deserialize)]
struct LastPlanFile {
    schema: u32,
    entries: Vec<SavedSelection>,
}

/// Result of reading `last_plan`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastPlanRead {
    Absent,
    Plan(SavedPlan),
    /// Unknown schema, truncated, or dated in the future: treated as no dry run, with a warning.
    Unreadable,
}

pub type NowFn = Arc<dyn Fn() -> i64 + Send + Sync>;

/// `PlanStore` over the state dir. Saves take `last_plan.lock`, merge through the pure `merge_saved`, and write
/// by temp file plus rename; an `ENOSPC` truncates `last_plan.reserve` and retries once.
pub struct RealPlanStore {
    dir: StateDir,
    now: NowFn,
}

impl RealPlanStore {
    pub fn new(dir: StateDir, now: NowFn) -> RealPlanStore {
        RealPlanStore { dir, now }
    }

    pub fn read(&self) -> LastPlanRead {
        let path = self.dir.paths.last_plan();
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return LastPlanRead::Absent,
            Err(_) => return LastPlanRead::Unreadable,
        };
        let now = (self.now)();
        match serde_json::from_slice::<LastPlanFile>(&bytes) {
            Ok(f)
                if f.schema == LAST_PLAN_SCHEMA && f.entries.iter().all(|e| e.saved_at <= now) =>
            {
                LastPlanRead::Plan(SavedPlan { entries: f.entries })
            }
            _ => LastPlanRead::Unreadable,
        }
    }

    /// The `last_plan unreadable` warning for the caller to print when [`read`](Self::read) said so.
    pub fn unreadable_warning(&self) -> Warning {
        Warning::LastPlanUnreadable {
            path: self.dir.paths.last_plan(),
        }
    }

    fn lock(&self) -> Option<File> {
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(PRIVATE)
            .open(self.dir.paths.last_plan_lock())
            .ok()?;
        chown_if(self.dir.owner, &self.dir.paths.last_plan_lock());
        let start = Instant::now();
        loop {
            match flock(&f, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Some(f),
                Err(_) if start.elapsed() < LOCK_WAIT => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return None,
            }
        }
    }

    fn write_once(&self, bytes: &[u8]) -> io::Result<()> {
        let tmp = self.dir.paths.dir.join("last_plan.tmp");
        let res = (|| {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(PRIVATE)
                .open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_data()?;
            chown_if(self.dir.owner, &tmp);
            fs::rename(&tmp, self.dir.paths.last_plan())
        })();
        if res.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        res
    }
}

impl PlanStore for RealPlanStore {
    fn load_last(&self) -> Option<SavedPlan> {
        match self.read() {
            LastPlanRead::Plan(p) => Some(p),
            LastPlanRead::Absent | LastPlanRead::Unreadable => None,
        }
    }

    fn save_merged(&self, new: Vec<SavedSelection>, euid: u32, now: i64) -> SaveOutcome {
        let Some(_lock) = self.lock() else {
            return SaveOutcome::NotSaved(LOCK_BUSY_ERRNO);
        };
        let existing = match self.read() {
            LastPlanRead::Plan(p) => p.entries,
            LastPlanRead::Absent | LastPlanRead::Unreadable => Vec::new(),
        };
        let merged = merge_saved(existing, new, euid, now);
        let Ok(bytes) = serde_json::to_vec(&LastPlanFile {
            schema: LAST_PLAN_SCHEMA,
            entries: merged,
        }) else {
            return SaveOutcome::NotSaved(5);
        };
        match self.write_once(&bytes) {
            Ok(()) => SaveOutcome::Saved,
            Err(e) if is_no_space(&e) => {
                let errno = e.raw_os_error().unwrap_or(ENOSPC);
                match release_reserve(&self.dir.paths.last_plan_reserve()) {
                    Ok(true) => match self.write_once(&bytes) {
                        Ok(()) => SaveOutcome::SavedAfterReserve,
                        Err(e) => SaveOutcome::NotSaved(e.raw_os_error().unwrap_or(errno)),
                    },
                    _ => SaveOutcome::NotSaved(errno),
                }
            }
            Err(e) => SaveOutcome::NotSaved(e.raw_os_error().unwrap_or(5)),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// outcome.jsonl

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutcomeMode {
    Plan,
    Apply,
    Status,
}

/// One run of any mode, with the verdict of the snapshot taken at its start (Story 2.3.5 reads it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeLine {
    pub schema: u32,
    pub ts: i64,
    pub run_id: String,
    pub mode: OutcomeMode,
    pub verdict: DiskVerdict,
    pub free_bytes: u64,
    pub unallocated_bytes: Option<u64>,
    /// The `DISK_CLEANUP_VIA` value, or null.
    pub via: Option<String>,
}

pub const OUTCOME_SCHEMA: u32 = 1;
/// Keeps a line under 200 bytes with ids of the usual length.
const OUTCOME_VIA_MAX: usize = 48;
const PRIVATE: u32 = 0o600;

/// Appends one line (at most 200 bytes) and rotates at 256 KiB, keeping one old generation. A failure is the
/// caller's `warning:`; it never changes the exit code. Refused runs (steps 1 to 4) must not call this.
pub fn append_outcome(
    paths: &StatePaths,
    owner: Option<Owner>,
    line: &OutcomeLine,
) -> io::Result<()> {
    let mut line = line.clone();
    if let Some(v) = line.via.as_mut() {
        let mut end = v.len().min(OUTCOME_VIA_MAX);
        while !v.is_char_boundary(end) {
            end -= 1;
        }
        v.truncate(end);
    }
    let mut s = serde_json::to_string(&line).map_err(io::Error::other)?;
    s.push('\n');
    let path = paths.outcome();
    if fs::metadata(&path).is_ok_and(|m| m.len() >= OUTCOME_ROTATE_BYTES) {
        fs::rename(&path, paths.dir.join("outcome.jsonl.1"))?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(PRIVATE)
        .open(&path)?;
    chown_if(owner, &path);
    f.write_all(s.as_bytes())
}

/// Reads a file whole, for tests and `audit verify` helpers that need small files.
pub fn read_small(path: &Path) -> io::Result<String> {
    let mut s = String::new();
    File::open(path)?.read_to_string(&mut s)?;
    Ok(s)
}

/// POSIX-shell single-quoting of a path for a printed command: bare when it is made of safe characters.
pub fn shell_quote_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:@,%".contains(c))
    {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

//! The delete engine entry point and its frozen contract (Story 0.1.3); the engine is Story 2.1.2.
//!
//! ```contract
//! // core (data). Derives are part of the contract: tests use assert_eq!, {:?} and Result::unwrap_err.
//! // VERIFIED (iteration 4, DeleteError re-VERIFIED in iteration 5 with the three new variants) for Identity, RootDev, AllocatedBytes, ApparentBytes, Freed, DeleteError, DeleteFailure in a scratch crate.
//! // Phase 4: CommandFailed gains a bounded `message`, so DeleteError and DeleteFailure lose Copy (Clone, PartialEq, Eq stay). VERIFIED in prototypes/command-failed (cargo test: 5 passed).
//! #[derive(Debug, Clone, PartialEq, Eq)]
//! pub struct StatInfo { pub ident: Identity, pub kind: FileKind, pub mode: u32, pub nlink: u64,
//!                       pub blocks: u64 /* 512-byte units */, pub size: u64,
//!                       pub mtime_secs: i64, pub mtime_nsec: u32, pub mnt_id: Option<u64>,
//!                       pub btime: Option<BirthTime> /* None where the filesystem has no birth time */ }
//! #[derive(Debug, Clone, Copy, PartialEq, Eq)]
//! pub enum FileKind { Dir, File, Symlink, Fifo, Socket, CharDev, BlockDev, Other }
//! #[derive(Debug, Clone, PartialEq, Eq)]
//! pub struct DirEntryInfo { pub name: OsString, pub kind: FileKind, pub ino: u64 }
//! #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)] pub struct Identity { pub dev: u64, pub ino: u64 }
//! impl Identity { pub fn of(m: &std::fs::Metadata) -> Identity;   // std::os::unix::fs::MetadataExt; plain core, no feature, no libc
//!                 pub fn on_dev(&self, d: RootDev) -> bool }
//! #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)] pub struct RootDev(pub u64);
//! #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)] pub struct AllocatedBytes(pub u64);
//! #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)] pub struct ApparentBytes(pub u64);
//! #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
//! pub struct Freed { pub attributed_bytes: AllocatedBytes, pub apparent_bytes: ApparentBytes,
//!                    pub entries_removed: u64, pub inodes_freed: u64 }
//! #[derive(Debug, Clone, PartialEq, Eq)]
//! pub struct DeleteFailure { pub error: DeleteError, pub freed: Freed }   // freed = what was counted before the failure
//! #[derive(Debug, Clone, PartialEq, Eq)]
//! pub enum DeleteError { IdentityMismatch, CrossDevice, Symlink, ProtectedPath, OutsideScope, Busy, NoSpace,
//!                        Io(i32) /* raw errno only */,
//!                        CommandFailed { status: i32, message: String /* first line, <= CMD_MSG_MAX bytes */ },
//!                        Refused, Timeout }
//! pub const CMD_MSG_MAX: usize = 200;
//! pub fn first_line_capped(raw: &str) -> String;   // first non-empty line, control chars -> ' ', cut on a char boundary, ends with "..." when cut
//! impl AbortFlag { pub fn new() -> AbortFlag; pub fn set(&self); pub fn is_set(&self) -> bool }  // + Debug, Default, Send + Sync
//! // ApplyToken derives nothing; Allowed and WorktreeRemoval derive Debug only (check-token-sites.sh enforces both).
//! // Explorer deletes (ADR-006, review C1/C2): `AllowedTarget::BelowScanRoot(ExplorerScope)`; an ExplorerScope is made only from an `Allowed`
//! // of kind ExplorerSelection, and `DeletePolicy::for_allowed_roots(&Allowed, RootDev, u64, &RootSet, ItemIdentity)` (root_mnt is a plain u64, not an Option;
//! // home, home_id and the scan roots come from the RootSet; the planned ItemIdentity must equal the witness's) is the constructor of that policy.
//! // `delete_tree` refuses a BelowScanRoot policy whose scope identity differs from `expect` (IdentityMismatch). `DeletePolicy::for_allowed` is the earlier,
//! // looser constructor, kept only until the pinned suites move to `for_allowed_roots`.
//!
//! // adapters::dirops
//! pub const DELETE_BATCH: usize = 1024;   // max subdirectory names a delete_tree level buffers; non-directories are unlinked as they stream
//! pub trait DirOps: Send + Sync {
//!     type Dir: Send + Sync;
//!     type ReadDir: Iterator<Item = io::Result<DirEntryInfo>> + Send;   // owns its own handle, so the caller may unlinkat on `d` while iterating
//!     fn open_root(&self, canonical_base: &Path) -> io::Result<Self::Dir>;      // O_DIRECTORY|O_NOFOLLOW on the last component; caller passes a canonical path
//!     fn open_dir(&self, parent: &Self::Dir, name: &OsStr) -> io::Result<Self::Dir>; // openat O_NOFOLLOW|O_DIRECTORY|O_CLOEXEC; name "." re-opens parent
//!     fn fstat(&self, d: &Self::Dir) -> io::Result<StatInfo>;
//!     fn stat_at(&self, d: &Self::Dir, name: &OsStr) -> io::Result<StatInfo>;    // fstatat AT_SYMLINK_NOFOLLOW: the per-file stat (blocks, nlink, ident)
//!     fn read_dir(&self, d: &Self::Dir) -> io::Result<Self::ReadDir>;            // streaming; d_type, lstat fallback on DT_UNKNOWN; O(1) memory per call
//!     fn unlinkat(&self, d: &Self::Dir, name: &OsStr, dir: bool) -> io::Result<()>;
//!     fn fchmod(&self, d: &Self::Dir, mode: u32) -> io::Result<()>;
//! }
//! // capabilities: private fields, only these two functions construct them
//! pub struct VerifiedRoot<D> { /* dir, abs_path */ }   impl<D> VerifiedRoot<D>   { pub fn dir(&self) -> &D; pub fn abs_path(&self) -> &Path }
//! pub struct VerifiedParent<D> { /* dir, abs_path */ } impl<D> VerifiedParent<D> { pub fn dir(&self) -> &D; pub fn abs_path(&self) -> &Path }
//! pub fn open_verified_root<D: DirOps>(ops: &D, canonical_base: &Path, expect: Identity)
//!                                      -> Result<VerifiedRoot<D::Dir>, DeleteError>;   // open_root, fstat, IdentityMismatch if ident != expect
//! pub fn open_parent<D: DirOps>(ops: &D, root: &VerifiedRoot<D::Dir>, rel_parent: &Path)
//!                               -> Result<VerifiedParent<D::Dir>, DeleteError>;        // abs_path = root.abs_path().join(rel_parent); empty rel_parent re-opens ".";
//!                               // rel_parent must be empty or relative with only normal components spelled exactly as its components
//!                               // join: `..`, `.`, a leading `/`, `a//b`, `a/./b` and a trailing `/` return Err(OutsideScope) before any open
//! pub fn delete_tree<D: DirOps>(ops: &D, parent: &VerifiedParent<D::Dir>, name: &OsStr, expect: Identity,
//!                               policy: &DeletePolicy, abort: &AbortFlag, proof: &ApplyToken)
//!                               -> Result<Freed, DeleteFailure>;
//!
//! // adapters::testing (feature test-support)
//! pub enum Op { OpenRoot, OpenDir, Fstat, StatAt, ReadDir, Unlinkat, Fchmod }
//! pub enum Event { AfterOpenDir, BeforeReadDir, BeforeUnlinkat }
//! pub enum At { Rel(PathBuf), Name(OsString), Any }
//! pub struct HookCtx { pub rel_path: PathBuf, pub base_abs: PathBuf, pub event: Event }
//! impl<D: DirOps> FaultDirOps<D> {                       // and: impl<D: DirOps> DirOps for FaultDirOps<D>, Dir = FaultDir<D::Dir>, ReadDir = FaultReadDir<D::ReadDir>
//!     pub fn wrap(inner: D) -> FaultDirOpsBuilder<D>;
//!     pub fn calls(&self, op: Op) -> usize;              // atomic count of calls to that Op, all threads
//! }
//! impl<D: DirOps> FaultDirOpsBuilder<D> {
//!     pub fn enospc_on_nth_unlinkat(self, n: usize) -> Self;               // 1-based, all threads
//!     pub fn errno_on(self, op: Op, at: At, errno: i32) -> Self;           // every matching call
//!     pub fn report_dev(self, at: At, dev: u64) -> Self;
//!     pub fn report_mnt_id(self, at: At, mnt_id: u64) -> Self;
//!     pub fn hook(self, event: Event, f: impl Fn(&HookCtx) + Send + Sync + 'static) -> Self;
//!     pub fn build(self) -> FaultDirOps<D>;
//! }
//! ```
//!
//! Invariants the implementation must keep (ADR-003):
//!
//! - I1 never follow a symlink
//! - I2 never cross `st_dev` (or `mnt_id`) of the root
//! - I3 never act on a path string after the fd is open
//! - I4 abort on `ENOSPC`, returning the partial `Freed`
//! - I5 hardlinks are counted once per `(dev, ino)`
//! - I6 refuse floor paths (rule F, `DeletePolicy::validate`)
//! - I7 idempotent on `ENOENT`
//! - I8 no stray unlink anywhere else

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;

use dunnage_core::{
    AbortFlag, AllocatedBytes, ApparentBytes, ApplyToken, DeleteError, DeleteFailure, DeletePolicy,
    FileKind, Freed, Identity, RootDev, StatInfo,
};

use crate::dirops::{map_open_error, DirOps, VerifiedParent, DELETE_BATCH, MAX_OPEN_LEVELS};

/// One directory being emptied. Frames are a stack, so depth costs heap, never call stack.
struct Frame<D: DirOps> {
    name: OsString,
    /// Recorded at descent; every later re-open must match it.
    ident: Identity,
    /// Blocks at descent, counted when the directory itself is removed.
    blocks: u64,
    /// `None` once evicted from the window of `MAX_OPEN_LEVELS` handles.
    handle: Option<D::Dir>,
    /// Subdirectory names found by the current scan, at most `DELETE_BATCH`.
    pending: Vec<OsString>,
    scanned: bool,
    /// Entries removed or newly failed since the scan began; a scan that touched anything is repeated.
    touched: u64,
    /// Names that failed at this level; never retried, and the level is not removed.
    skip: HashSet<OsString>,
    failed: bool,
}

enum Step {
    /// The entry is already gone (`ENOENT`): success by idempotence (I7).
    Vanished,
    /// Abort class: the whole call ends.
    Fatal(DeleteError),
    Failed(DeleteError),
}

struct Engine<'a, D: DirOps> {
    ops: &'a D,
    base: &'a D::Dir,
    policy: &'a DeletePolicy,
    abort: &'a AbortFlag,
    frames: Vec<Frame<D>>,
    freed: Freed,
    /// Hardlinked inodes seen inside the tree: `(names unlinked, names at first sight)`.
    links: HashMap<(u64, u64), (u64, u64)>,
    first_err: Option<DeleteError>,
}

fn is_abort_errno(n: i32) -> bool {
    matches!(n, libc::ENOSPC | libc::EDQUOT | libc::EIO)
}

fn classify(e: &io::Error) -> Step {
    match e.raw_os_error() {
        Some(libc::ENOENT) => Step::Vanished,
        Some(n) if is_abort_errno(n) => Step::Fatal(DeleteError::NoSpace),
        _ => Step::Failed(map_open_error(e)),
    }
}

fn failure(error: DeleteError, freed: Freed) -> DeleteFailure {
    DeleteFailure { error, freed }
}

/// Removes `name` under `parent`; see the module contract. Iterative, holds at most `MAX_OPEN_LEVELS`
/// directory handles, and never follows a symlink or leaves `policy.root_dev` / `policy.root_mnt`.
///
/// Counting (Glossary "Counting rule"): every name removed counts one entry, directories included; an
/// inode adds `blocks * 512` once, a hardlinked one only when its last name inside the tree goes.
/// A skipped mount point, a busy or unreadable directory ends the call in `Err` after the rest is done;
/// an abort-class errno (`ENOSPC`, `EDQUOT`, `EIO`) sets `abort` and ends it at once.
#[allow(unused_variables)]
pub fn delete_tree<D: DirOps>(
    ops: &D,
    parent: &VerifiedParent<D::Dir>,
    name: &OsStr,
    expect: Identity,
    policy: &DeletePolicy,
    abort: &AbortFlag,
    proof: &ApplyToken,
) -> Result<Freed, DeleteFailure> {
    let _ = proof;
    let none = Freed::default();
    policy.validate(name).map_err(|e| failure(e, none))?;
    if parent.abs_path().join(name) != policy.target {
        return Err(failure(DeleteError::OutsideScope, none));
    }
    if abort.is_set() {
        return Err(failure(DeleteError::Refused, none));
    }
    let mut engine = Engine {
        ops,
        base: parent.dir(),
        policy,
        abort,
        frames: Vec::new(),
        freed: none,
        links: HashMap::new(),
        first_err: None,
    };
    engine.check_sibling_marker()?;
    let target = match ops.stat_at(parent.dir(), name) {
        Ok(st) => st,
        Err(e) => {
            return match classify(&e) {
                Step::Vanished => Ok(none),
                Step::Fatal(err) | Step::Failed(err) => Err(failure(err, none)),
            }
        }
    };
    if target.kind != FileKind::Dir {
        return Err(failure(DeleteError::Symlink, none));
    }
    if target.ident != expect {
        return Err(failure(DeleteError::IdentityMismatch, none));
    }
    // An explorer delete: `expect` must be the identity the witness was minted for (review 2, N4), so a fresh
    // guard identity cannot stand in for the planned one.
    if !policy.witness_matches(expect) {
        return Err(failure(DeleteError::IdentityMismatch, none));
    }
    if target.ident == policy.home_id {
        return Err(failure(DeleteError::ProtectedPath, none));
    }
    if let Some(err) = engine.crossing(&target) {
        return Err(failure(err, none));
    }
    engine.open_frame(None, name, &target)?;
    engine.run()
}

fn crosses(st: &StatInfo, root_dev: RootDev, root_mnt: Option<u64>) -> bool {
    !st.ident.on_dev(root_dev) || matches!((root_mnt, st.mnt_id), (Some(r), Some(m)) if r != m)
}

impl<D: DirOps> Engine<'_, D> {
    fn fail_now(&self, error: DeleteError) -> DeleteFailure {
        failure(error, self.freed)
    }

    fn crossing(&self, st: &StatInfo) -> Option<DeleteError> {
        crosses(st, self.policy.root_dev, self.policy.root_mnt).then_some(DeleteError::CrossDevice)
    }

    fn check_sibling_marker(&self) -> Result<(), DeleteFailure> {
        let markers = &self.policy.require_sibling_any;
        if markers.is_empty() {
            return Ok(());
        }
        // The marker is looked up next to the target, in the verified parent.
        let found = markers
            .iter()
            .any(|m| self.ops.stat_at(self.base, m).is_ok());
        if found {
            Ok(())
        } else {
            Err(self.fail_now(DeleteError::OutsideScope))
        }
    }

    /// Opens `name` below frame `at` (or below the verified parent) and pushes it as a new level.
    fn open_frame(
        &mut self,
        at: Option<usize>,
        name: &OsStr,
        st: &StatInfo,
    ) -> Result<(), DeleteFailure> {
        let handle = {
            let parent = match at {
                Some(i) => self.frames[i].handle.as_ref(),
                None => Some(self.base),
            };
            let Some(parent) = parent else {
                return Err(self.fail_now(DeleteError::IdentityMismatch));
            };
            self.ops
                .open_dir(parent, name)
                .map_err(|e| self.fail_now(map_open_error(&e)))?
        };
        let opened = self
            .ops
            .fstat(&handle)
            .map_err(|e| self.fail_now(map_open_error(&e)))?;
        if opened.ident != st.ident {
            return Err(self.fail_now(DeleteError::IdentityMismatch));
        }
        if self.policy.restore_owner_write && opened.mode & 0o700 != 0o700 {
            self.ops
                .fchmod(&handle, opened.mode | 0o700)
                .map_err(|e| self.fail_now(map_open_error(&e)))?;
        }
        self.frames.push(Frame {
            name: name.to_os_string(),
            ident: st.ident,
            blocks: st.blocks,
            handle: Some(handle),
            pending: Vec::new(),
            scanned: false,
            touched: 0,
            skip: HashSet::new(),
            failed: false,
        });
        let n = self.frames.len();
        if n > MAX_OPEN_LEVELS {
            self.frames[n - 1 - MAX_OPEN_LEVELS].handle = None;
        }
        Ok(())
    }

    /// Re-opens an evicted level from the nearest retained ancestor, comparing identities with the ones
    /// recorded at descent, and keeps the last `MAX_OPEN_LEVELS` of the chain.
    fn ensure_handle(&mut self, k: usize) -> Result<(), DeleteFailure> {
        if self.frames[k].handle.is_some() {
            return Ok(());
        }
        let mut s = k;
        while s > 0 && self.frames[s - 1].handle.is_none() {
            s -= 1;
        }
        let keep = |i: usize| i + MAX_OPEN_LEVELS > k;
        let mut temp: Option<D::Dir> = None;
        for i in s..=k {
            let opened = {
                let parent: &D::Dir = if i == 0 {
                    self.base
                } else if i == s || keep(i - 1) {
                    match self.frames[i - 1].handle.as_ref() {
                        Some(h) => h,
                        None => return Err(self.fail_now(DeleteError::IdentityMismatch)),
                    }
                } else {
                    match temp.as_ref() {
                        Some(h) => h,
                        None => return Err(self.fail_now(DeleteError::IdentityMismatch)),
                    }
                };
                let h = self
                    .ops
                    .open_dir(parent, &self.frames[i].name)
                    .map_err(|e| self.fail_now(map_open_error(&e)))?;
                let st = self
                    .ops
                    .fstat(&h)
                    .map_err(|e| self.fail_now(map_open_error(&e)))?;
                if st.ident != self.frames[i].ident {
                    return Err(self.fail_now(DeleteError::IdentityMismatch));
                }
                h
            };
            if keep(i) {
                self.frames[i].handle = Some(opened);
                temp = None;
            } else {
                temp = Some(opened);
            }
        }
        Ok(())
    }

    fn note_failure(&mut self, level: usize, name: OsString, error: DeleteError) {
        self.first_err.get_or_insert(error);
        let f = &mut self.frames[level];
        f.failed = true;
        f.touched += 1;
        f.skip.insert(name);
    }

    fn check_abort(&self) -> Result<(), DeleteFailure> {
        if self.abort.is_set() {
            return Err(self.fail_now(DeleteError::Refused));
        }
        Ok(())
    }

    fn fatal(&self, error: DeleteError) -> DeleteFailure {
        self.abort.set();
        self.fail_now(error)
    }

    fn count_name(&mut self, st: &StatInfo) {
        self.freed.entries_removed += 1;
        self.freed.apparent_bytes = ApparentBytes(self.freed.apparent_bytes.0 + st.size);
        // Earlier names are already unlinked, so a later name's own `nlink` is too small: the count of names
        // is the one recorded when the inode was first seen.
        let key = (st.ident.dev, st.ident.ino);
        let last_link = match self.links.get_mut(&key) {
            Some(e) => {
                e.0 += 1;
                let all_in_tree = e.0 >= e.1;
                if all_in_tree {
                    self.links.remove(&key);
                }
                all_in_tree
            }
            None if st.nlink <= 1 => true,
            None => {
                self.links.insert(key, (1, st.nlink));
                false
            }
        };
        if last_link {
            self.count_inode(st.blocks);
        }
    }

    fn count_inode(&mut self, blocks: u64) {
        self.freed.inodes_freed += 1;
        self.freed.attributed_bytes = AllocatedBytes(self.freed.attributed_bytes.0 + blocks * 512);
    }

    /// Streams one listing of the top level, unlinking non-directories and buffering subdirectory names.
    fn scan(&mut self, level: usize) -> Result<(), DeleteFailure> {
        let Some(handle) = self.frames[level].handle.take() else {
            return Err(self.fail_now(DeleteError::IdentityMismatch));
        };
        let result = self.scan_with(level, &handle);
        self.frames[level].handle = Some(handle);
        result?;
        self.frames[level].scanned = true;
        Ok(())
    }

    fn scan_with(&mut self, level: usize, handle: &D::Dir) -> Result<(), DeleteFailure> {
        let iter = match self.ops.read_dir(handle) {
            Ok(it) => it,
            Err(e) => {
                match classify(&e) {
                    Step::Vanished => {}
                    Step::Fatal(err) => return Err(self.fatal(err)),
                    Step::Failed(err) => {
                        self.first_err.get_or_insert(err);
                        self.frames[level].failed = true;
                    }
                }
                return Ok(());
            }
        };
        for item in iter {
            self.check_abort()?;
            let entry = match item {
                Ok(en) => en,
                Err(e) => {
                    match classify(&e) {
                        Step::Vanished => {}
                        Step::Fatal(err) => return Err(self.fatal(err)),
                        Step::Failed(err) => {
                            self.first_err.get_or_insert(err);
                            self.frames[level].failed = true;
                        }
                    }
                    break;
                }
            };
            if self.frames[level].skip.contains(&entry.name) {
                continue;
            }
            if entry.kind == FileKind::Dir {
                if self.frames[level].pending.len() < DELETE_BATCH {
                    self.frames[level].pending.push(entry.name);
                }
                continue;
            }
            self.remove_leaf(level, handle, entry.name)?;
            if self.frames[level].skip.len() > DELETE_BATCH {
                self.frames[level].pending.clear();
                break;
            }
        }
        Ok(())
    }

    /// `stat_at` for the accounting, then `unlinkat` by name on the handle (never a path).
    fn remove_leaf(
        &mut self,
        level: usize,
        handle: &D::Dir,
        name: OsString,
    ) -> Result<(), DeleteFailure> {
        let st = match self.ops.stat_at(handle, &name) {
            Ok(st) => st,
            Err(e) => {
                return match classify(&e) {
                    Step::Vanished => {
                        self.frames[level].touched += 1;
                        Ok(())
                    }
                    Step::Fatal(err) => Err(self.fatal(err)),
                    Step::Failed(err) => {
                        self.note_failure(level, name, err);
                        Ok(())
                    }
                }
            }
        };
        if st.kind == FileKind::Dir {
            if self.frames[level].pending.len() < DELETE_BATCH {
                self.frames[level].pending.push(name);
            }
            return Ok(());
        }
        self.check_abort()?;
        match self.ops.unlinkat(handle, &name, false) {
            Ok(()) => {
                self.count_name(&st);
                self.frames[level].touched += 1;
            }
            Err(e) => match classify(&e) {
                Step::Vanished => self.frames[level].touched += 1,
                Step::Fatal(err) => return Err(self.fatal(err)),
                Step::Failed(err) => self.note_failure(level, name, err),
            },
        }
        Ok(())
    }

    /// Descends into the next buffered subdirectory of `level`, or records why it cannot.
    fn descend(&mut self, level: usize, name: OsString) -> Result<(), DeleteFailure> {
        let st = {
            let Some(handle) = self.frames[level].handle.as_ref() else {
                return Err(self.fail_now(DeleteError::IdentityMismatch));
            };
            self.ops.stat_at(handle, &name)
        };
        let st = match st {
            Ok(st) => st,
            Err(e) => {
                match classify(&e) {
                    Step::Vanished => self.frames[level].touched += 1,
                    Step::Fatal(err) => return Err(self.fatal(err)),
                    Step::Failed(err) => self.note_failure(level, name, err),
                }
                return Ok(());
            }
        };
        if st.kind != FileKind::Dir {
            // Replaced since the listing: leave it to the next scan, which sees its new kind.
            self.frames[level].touched += 1;
            return Ok(());
        }
        if let Some(err) = self.crossing(&st) {
            self.note_failure(level, name, err);
            return Ok(());
        }
        if self.frames.len() > self.policy.max_depth as usize {
            self.note_failure(level, name, DeleteError::Io(libc::ENAMETOOLONG));
            return Ok(());
        }
        match self.open_frame(Some(level), &name, &st) {
            Ok(()) => Ok(()),
            Err(f) => match f.error {
                DeleteError::NoSpace => Err(self.fatal(DeleteError::NoSpace)),
                DeleteError::Io(libc::ENOENT) => {
                    self.frames[level].touched += 1;
                    Ok(())
                }
                err => {
                    self.note_failure(level, name, err);
                    Ok(())
                }
            },
        }
    }

    /// The top level is empty: remove it from its parent, or hand its failure to the parent.
    fn finish(&mut self) -> Result<bool, DeleteFailure> {
        let level = self.frames.len() - 1;
        if !self.frames[level].failed {
            self.check_abort()?;
            if level > 0 {
                self.ensure_handle(level - 1)?;
            }
            let outcome = {
                let parent: &D::Dir = if level == 0 {
                    self.base
                } else {
                    match self.frames[level - 1].handle.as_ref() {
                        Some(h) => h,
                        None => return Err(self.fail_now(DeleteError::IdentityMismatch)),
                    }
                };
                self.ops.unlinkat(parent, &self.frames[level].name, true)
            };
            match outcome {
                Ok(()) => {
                    let blocks = self.frames[level].blocks;
                    self.freed.entries_removed += 1;
                    self.count_inode(blocks);
                }
                Err(e) => match e.raw_os_error() {
                    Some(libc::ENOENT) => {}
                    Some(libc::ENOTEMPTY | libc::EEXIST) => {
                        self.frames[level].failed = true;
                        self.first_err.get_or_insert(DeleteError::Busy);
                    }
                    Some(libc::ENOTDIR) => {
                        self.frames[level].failed = true;
                        self.first_err.get_or_insert(DeleteError::IdentityMismatch);
                    }
                    Some(n) if is_abort_errno(n) => return Err(self.fatal(DeleteError::NoSpace)),
                    _ => {
                        self.frames[level].failed = true;
                        self.first_err.get_or_insert(map_open_error(&e));
                    }
                },
            }
        }
        let done = self.frames.pop();
        let Some(done) = done else { return Ok(true) };
        if level == 0 {
            return Ok(true);
        }
        if done.failed {
            let first = self.first_err.clone().unwrap_or(DeleteError::Busy);
            self.note_failure(level - 1, done.name, first);
        } else {
            self.frames[level - 1].touched += 1;
        }
        Ok(false)
    }

    fn run(mut self) -> Result<Freed, DeleteFailure> {
        loop {
            self.check_abort()?;
            let level = self.frames.len() - 1;
            self.ensure_handle(level)?;
            if let Some(name) = self.frames[level].pending.pop() {
                self.descend(level, name)?;
            } else if !self.frames[level].scanned {
                self.frames[level].touched = 0;
                self.scan(level)?;
            } else if self.frames[level].touched > 0
                && self.frames[level].skip.len() <= DELETE_BATCH
            {
                self.frames[level].scanned = false;
            } else if self.finish()? {
                break;
            }
        }
        match self.first_err {
            None => Ok(self.freed),
            Some(error) => Err(failure(error, self.freed)),
        }
    }
}

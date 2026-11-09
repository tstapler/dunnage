//! The delete engine entry point. Frozen contract (Story 0.1.3); the body of `delete_tree` is `todo!()`
//! until Story 2.1.2.
//!
//! ```contract
//! // core (data). Derives are part of the contract: tests use assert_eq!, {:?} and Result::unwrap_err.
//! // VERIFIED (iteration 4, DeleteError re-VERIFIED in iteration 5 with the three new variants) for Identity, RootDev, AllocatedBytes, ApparentBytes, Freed, DeleteError, DeleteFailure in a scratch crate.
//! // Phase 4: CommandFailed gains a bounded `message`, so DeleteError and DeleteFailure lose Copy (Clone, PartialEq, Eq stay). VERIFIED in prototypes/command-failed (cargo test: 5 passed).
//! #[derive(Debug, Clone, PartialEq, Eq)]
//! pub struct StatInfo { pub ident: Identity, pub kind: FileKind, pub mode: u32, pub nlink: u64,
//!                       pub blocks: u64 /* 512-byte units */, pub size: u64,
//!                       pub mtime_secs: i64, pub mtime_nsec: u32, pub mnt_id: Option<u64> }
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

use std::ffi::OsStr;

use disk_cleanup_core::{AbortFlag, ApplyToken, DeleteFailure, DeletePolicy, Freed, Identity};

use crate::dirops::{DirOps, VerifiedParent};

/// Removes `name` under `parent`. Frozen stub: Story 2.1.2 implements it.
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
    todo!("Story 2.1.2")
}

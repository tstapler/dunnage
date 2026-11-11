//! The syscall seam under the delete engine: [`DirOps`], the capabilities [`VerifiedRoot`] and
//! [`VerifiedParent`], and [`RealDirOps`]. The frozen text of this contract is the `contract` block in
//! [`crate::delete`].

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use disk_cleanup_core::{ApplyToken, DeleteError, DirEntryInfo, FileKind, Identity, StatInfo};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};

/// Max subdirectory names `delete_tree` buffers per open level; non-directories are unlinked as they stream.
pub const DELETE_BATCH: usize = 1024;

/// Directory handles `delete_tree` keeps open at once; deeper ancestors are re-opened by identity-checked
/// `open_dir` when the walk climbs back (Story 2.1.2). Not part of the frozen contract.
pub const MAX_OPEN_LEVELS: usize = 64;

pub trait DirOps: Send + Sync {
    type Dir: Send + Sync;
    /// Owns its own handle, so the caller may `unlinkat` on the directory while iterating.
    type ReadDir: Iterator<Item = io::Result<DirEntryInfo>> + Send;
    /// `O_DIRECTORY|O_NOFOLLOW` on the last component; the caller passes a canonical path.
    fn open_root(&self, canonical_base: &Path) -> io::Result<Self::Dir>;
    /// `openat` with `O_NOFOLLOW|O_DIRECTORY|O_CLOEXEC`; name `.` re-opens `parent`.
    fn open_dir(&self, parent: &Self::Dir, name: &OsStr) -> io::Result<Self::Dir>;
    fn fstat(&self, d: &Self::Dir) -> io::Result<StatInfo>;
    /// `fstatat` with `AT_SYMLINK_NOFOLLOW`: the per-file stat (blocks, nlink, ident).
    fn stat_at(&self, d: &Self::Dir, name: &OsStr) -> io::Result<StatInfo>;
    /// Streaming; `d_type` with an `lstat` fallback on `DT_UNKNOWN`; O(1) memory per call.
    fn read_dir(&self, d: &Self::Dir) -> io::Result<Self::ReadDir>;
    fn unlinkat(&self, d: &Self::Dir, name: &OsStr, dir: bool) -> io::Result<()>;
    fn fchmod(&self, d: &Self::Dir, mode: u32) -> io::Result<()>;
}

/// A directory opened by `open_root` whose identity was compared with the one the caller recorded.
pub struct VerifiedRoot<D> {
    dir: D,
    abs_path: PathBuf,
}

impl<D> VerifiedRoot<D> {
    pub fn dir(&self) -> &D {
        &self.dir
    }
    pub fn abs_path(&self) -> &Path {
        &self.abs_path
    }
}

/// A directory reached from a [`VerifiedRoot`] by a component-by-component `O_NOFOLLOW` walk.
pub struct VerifiedParent<D> {
    dir: D,
    abs_path: PathBuf,
}

impl<D> VerifiedParent<D> {
    pub fn dir(&self) -> &D {
        &self.dir
    }
    pub fn abs_path(&self) -> &Path {
        &self.abs_path
    }
}

/// Raised by [`RealDirOps`] when a mutating call lands outside its confinement root.
#[derive(Debug)]
pub struct ScopeViolation;

impl std::fmt::Display for ScopeViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("mutating call outside the confinement root")
    }
}

impl std::error::Error for ScopeViolation {}

fn scope_violation() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, ScopeViolation)
}

pub fn is_scope_violation(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<ScopeViolation>)
}

/// `""`, or only `Normal` components that re-join to exactly the input.
pub(crate) fn rel_is_clean(rel: &Path) -> bool {
    if rel.as_os_str().is_empty() {
        return true;
    }
    let mut joined = PathBuf::new();
    for c in rel.components() {
        match c {
            Component::Normal(n) => joined.push(n),
            _ => return false,
        }
    }
    joined.as_os_str() == rel.as_os_str()
}

pub(crate) fn abs_is_clean(p: &Path) -> bool {
    p == Path::new("/")
        || p.strip_prefix("/")
            .is_ok_and(|rel| p.is_absolute() && rel_is_clean(rel))
}

/// Maps an error from an `open` call: a symlink or a non-directory is [`DeleteError::Symlink`].
pub(crate) fn map_open_error(e: &io::Error) -> DeleteError {
    if is_scope_violation(e) {
        return DeleteError::OutsideScope;
    }
    match e.raw_os_error() {
        Some(libc::ELOOP | libc::ENOTDIR) => DeleteError::Symlink,
        Some(libc::ENOSPC) => DeleteError::NoSpace,
        Some(n) => DeleteError::Io(n),
        None => DeleteError::Io(libc::EIO),
    }
}

pub fn open_verified_root<D: DirOps>(
    ops: &D,
    canonical_base: &Path,
    expect: Identity,
) -> Result<VerifiedRoot<D::Dir>, DeleteError> {
    let dir = ops
        .open_root(canonical_base)
        .map_err(|e| map_open_error(&e))?;
    let st = ops.fstat(&dir).map_err(|e| map_open_error(&e))?;
    if st.ident != expect {
        return Err(DeleteError::IdentityMismatch);
    }
    Ok(VerifiedRoot {
        dir,
        abs_path: canonical_base.to_path_buf(),
    })
}

pub fn open_parent<D: DirOps>(
    ops: &D,
    root: &VerifiedRoot<D::Dir>,
    rel_parent: &Path,
) -> Result<VerifiedParent<D::Dir>, DeleteError> {
    if !rel_is_clean(rel_parent) {
        return Err(DeleteError::OutsideScope);
    }
    let abs_path = root.abs_path.join(rel_parent);
    let mut comps = rel_parent.components();
    let Some(Component::Normal(first)) = comps.next() else {
        let dir = ops
            .open_dir(&root.dir, OsStr::new("."))
            .map_err(|e| map_open_error(&e))?;
        return Ok(VerifiedParent { dir, abs_path });
    };
    let mut cur = ops
        .open_dir(&root.dir, first)
        .map_err(|e| map_open_error(&e))?;
    for c in comps {
        let next = ops
            .open_dir(&cur, c.as_os_str())
            .map_err(|e| map_open_error(&e))?;
        cur = next;
    }
    Ok(VerifiedParent { dir: cur, abs_path })
}

/// Real `openat`-family implementation.
pub struct RealDirOps {
    /// `None` is unconfined. `Some` refuses `unlinkat`/`fchmod` on handles whose lexical path is outside it.
    root: Option<PathBuf>,
    refuse_all: bool,
}

/// A directory fd plus the lexical absolute path it was opened at (used only for confinement).
pub struct RealDir {
    fd: OwnedFd,
    path: PathBuf,
}

impl RealDirOps {
    /// `root` must be canonical and absolute; otherwise this panics in debug and returns a value that
    /// refuses everything in release.
    pub fn confined_to(root: &Path) -> RealDirOps {
        let ok = abs_is_clean(root);
        debug_assert!(ok, "RealDirOps::confined_to needs a clean absolute path");
        RealDirOps {
            root: Some(root.to_path_buf()),
            refuse_all: !ok,
        }
    }

    /// Callable only from `cli/src/wiring/confine.rs` (`scripts/check-token-sites.sh`).
    pub fn unconfined(_proof: &ApplyToken) -> RealDirOps {
        RealDirOps {
            root: None,
            refuse_all: false,
        }
    }

    fn check_read(&self) -> io::Result<()> {
        if self.refuse_all {
            return Err(scope_violation());
        }
        Ok(())
    }

    fn check_mutation(&self, d: &RealDir) -> io::Result<()> {
        self.check_read()?;
        match &self.root {
            Some(root) if !d.path.starts_with(root) => Err(scope_violation()),
            _ => Ok(()),
        }
    }
}

/// One path component, not `..`, not containing `/`.
fn check_name(name: &OsStr) -> io::Result<()> {
    let b = name.as_bytes();
    if b.is_empty() || b == b".." || b.contains(&b'/') {
        return Err(scope_violation());
    }
    Ok(())
}

fn errno(e: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

fn kind_of(t: FileType) -> FileKind {
    match t {
        FileType::Directory => FileKind::Dir,
        FileType::RegularFile => FileKind::File,
        FileType::Symlink => FileKind::Symlink,
        FileType::Fifo => FileKind::Fifo,
        FileType::Socket => FileKind::Socket,
        FileType::CharacterDevice => FileKind::CharDev,
        FileType::BlockDevice => FileKind::BlockDev,
        FileType::Unknown => FileKind::Other,
    }
}

#[allow(clippy::unnecessary_cast)] // widths differ on non-Linux targets
fn from_stat(st: &rustix::fs::Stat) -> StatInfo {
    StatInfo {
        ident: Identity {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
        },
        kind: kind_of(FileType::from_raw_mode(st.st_mode as rustix::fs::RawMode)),
        mode: st.st_mode as u32 & 0o7777,
        nlink: st.st_nlink as u64,
        blocks: st.st_blocks as u64,
        size: st.st_size as u64,
        mtime_secs: st.st_mtime as i64,
        mtime_nsec: st.st_mtime_nsec as u32,
        mnt_id: None,
    }
}

// `AtFlags::EMPTY_PATH` exists only on Linux/Android; elsewhere `plain_stat` already picks `fstat` for an empty name.
#[cfg(any(target_os = "linux", target_os = "android"))]
const FSTAT_FLAGS: AtFlags = AtFlags::EMPTY_PATH;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const FSTAT_FLAGS: AtFlags = AtFlags::empty();

#[cfg(target_os = "linux")]
fn stat_info(fd: &OwnedFd, name: &OsStr, flags: AtFlags) -> io::Result<StatInfo> {
    use rustix::fs::StatxFlags;
    let mask = StatxFlags::BASIC_STATS | StatxFlags::MNT_ID;
    match rustix::fs::statx(fd, name, flags, mask) {
        Ok(sx) => Ok(StatInfo {
            ident: Identity {
                dev: rustix::fs::makedev(sx.stx_dev_major, sx.stx_dev_minor),
                ino: sx.stx_ino,
            },
            kind: kind_of(FileType::from_raw_mode(
                u32::from(sx.stx_mode) as rustix::fs::RawMode
            )),
            mode: u32::from(sx.stx_mode) & 0o7777,
            nlink: u64::from(sx.stx_nlink),
            blocks: sx.stx_blocks,
            size: sx.stx_size,
            mtime_secs: sx.stx_mtime.tv_sec,
            mtime_nsec: sx.stx_mtime.tv_nsec,
            mnt_id: (sx.stx_mask & StatxFlags::MNT_ID.bits() != 0).then_some(sx.stx_mnt_id),
        }),
        Err(rustix::io::Errno::NOSYS | rustix::io::Errno::INVAL) => plain_stat(fd, name, flags),
        Err(e) => Err(errno(e)),
    }
}

/// The mount id of the entry at `path` (`statx` `STATX_MNT_ID`, `AT_SYMLINK_NOFOLLOW`), `None` where the
/// kernel does not report one or the path cannot be read. It lets the explorer's read-only `Fs::lstat` carry
/// the same `mnt_id` the engine compares (review C9).
#[cfg(target_os = "linux")]
pub fn mount_id_of(path: &Path) -> Option<u64> {
    use rustix::fs::StatxFlags;
    let sx = rustix::fs::statx(
        rustix::fs::CWD,
        path,
        AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::MNT_ID,
    )
    .ok()?;
    (sx.stx_mask & StatxFlags::MNT_ID.bits() != 0).then_some(sx.stx_mnt_id)
}

/// The `statfs` filesystem type (`f_type`) at `path`, `None` when it cannot be read.
#[cfg(target_os = "linux")]
#[allow(clippy::unnecessary_cast, clippy::cast_sign_loss)]
pub fn fs_type_of(path: &Path) -> Option<u64> {
    rustix::fs::statfs(path).ok().map(|s| s.f_type as u64)
}

#[cfg(not(target_os = "linux"))]
pub fn fs_type_of(_path: &Path) -> Option<u64> {
    None
}

#[cfg(not(target_os = "linux"))]
pub fn mount_id_of(_path: &Path) -> Option<u64> {
    None
}

#[cfg(not(target_os = "linux"))]
fn stat_info(fd: &OwnedFd, name: &OsStr, flags: AtFlags) -> io::Result<StatInfo> {
    plain_stat(fd, name, flags)
}

fn plain_stat(fd: &OwnedFd, name: &OsStr, flags: AtFlags) -> io::Result<StatInfo> {
    let st = if name.is_empty() {
        rustix::fs::fstat(fd)
    } else {
        rustix::fs::statat(fd, name, flags)
    }
    .map_err(errno)?;
    Ok(from_stat(&st))
}

pub struct RealReadDir {
    dir: rustix::fs::Dir,
}

impl Iterator for RealReadDir {
    type Item = io::Result<DirEntryInfo>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry = match self.dir.read()? {
                Ok(e) => e,
                Err(e) => return Some(Err(errno(e))),
            };
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let name = OsString::from_vec(bytes.to_vec());
            let kind = match entry.file_type() {
                FileType::Unknown => {
                    let fd = match self.dir.fd() {
                        Ok(fd) => fd,
                        Err(e) => return Some(Err(errno(e))),
                    };
                    match rustix::fs::statat(fd, name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW) {
                        Ok(st) => from_stat(&st).kind,
                        Err(e) => return Some(Err(errno(e))),
                    }
                }
                t => kind_of(t),
            };
            return Some(Ok(DirEntryInfo {
                name,
                kind,
                ino: entry.ino(),
            }));
        }
    }
}

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

impl DirOps for RealDirOps {
    type Dir = RealDir;
    type ReadDir = RealReadDir;

    fn open_root(&self, canonical_base: &Path) -> io::Result<RealDir> {
        self.check_read()?;
        let fd = rustix::fs::open(canonical_base, DIR_FLAGS, Mode::empty()).map_err(errno)?;
        Ok(RealDir {
            fd,
            path: canonical_base.to_path_buf(),
        })
    }

    fn open_dir(&self, parent: &RealDir, name: &OsStr) -> io::Result<RealDir> {
        self.check_read()?;
        let path = if name == OsStr::new(".") {
            parent.path.clone()
        } else {
            check_name(name)?;
            parent.path.join(name)
        };
        let fd = rustix::fs::openat(&parent.fd, name, DIR_FLAGS, Mode::empty()).map_err(errno)?;
        Ok(RealDir { fd, path })
    }

    fn fstat(&self, d: &RealDir) -> io::Result<StatInfo> {
        self.check_read()?;
        stat_info(&d.fd, OsStr::new(""), FSTAT_FLAGS)
    }

    fn stat_at(&self, d: &RealDir, name: &OsStr) -> io::Result<StatInfo> {
        self.check_read()?;
        check_name(name)?;
        stat_info(&d.fd, name, AtFlags::SYMLINK_NOFOLLOW)
    }

    fn read_dir(&self, d: &RealDir) -> io::Result<RealReadDir> {
        self.check_read()?;
        let dir = rustix::fs::Dir::read_from(&d.fd).map_err(errno)?;
        Ok(RealReadDir { dir })
    }

    fn unlinkat(&self, d: &RealDir, name: &OsStr, dir: bool) -> io::Result<()> {
        self.check_mutation(d)?;
        check_name(name)?;
        let flags = if dir {
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        rustix::fs::unlinkat(&d.fd, name, flags).map_err(errno)
    }

    fn fchmod(&self, d: &RealDir, mode: u32) -> io::Result<()> {
        self.check_mutation(d)?;
        rustix::fs::fchmod(&d.fd, Mode::from_raw_mode(mode as rustix::fs::RawMode)).map_err(errno)
    }
}

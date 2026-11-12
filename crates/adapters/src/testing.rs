//! Fault-injection wrapper over any [`DirOps`] (feature `test-support`). The exact public surface is
//! frozen by the `contract` block in [`crate::delete`].

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use dunnage_core::{DirEntryInfo, StatInfo};

use crate::dirops::DirOps;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    OpenRoot,
    OpenDir,
    Fstat,
    StatAt,
    ReadDir,
    Unlinkat,
    Fchmod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    AfterOpenDir,
    BeforeReadDir,
    BeforeUnlinkat,
}

/// `Rel` is relative to the base given to `open_root`; `Name` matches an entry name at any depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum At {
    Rel(PathBuf),
    Name(OsString),
    Any,
}

#[derive(Debug, Clone)]
pub struct HookCtx {
    pub rel_path: PathBuf,
    pub base_abs: PathBuf,
    pub event: Event,
}

type Hook = Arc<dyn Fn(&HookCtx) + Send + Sync + 'static>;

struct Rules {
    enospc_nth_unlinkat: Option<usize>,
    errnos: Vec<(Op, At, i32)>,
    devs: Vec<(At, u64)>,
    mnt_ids: Vec<(At, u64)>,
    hooks: Vec<(Event, Hook)>,
}

pub struct FaultDirOps<D> {
    inner: D,
    rules: Rules,
    counts: [AtomicUsize; 7],
}

pub struct FaultDirOpsBuilder<D> {
    inner: D,
    rules: Rules,
}

/// A directory handle that remembers where it is relative to the base.
pub struct FaultDir<T> {
    inner: T,
    rel: PathBuf,
    base_abs: PathBuf,
}

pub struct FaultReadDir<R> {
    inner: R,
}

impl<R: Iterator<Item = io::Result<DirEntryInfo>>> Iterator for FaultReadDir<R> {
    type Item = io::Result<DirEntryInfo>;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

fn matches(at: &At, rel: &Path, name: Option<&OsStr>) -> bool {
    match at {
        At::Any => true,
        At::Rel(p) => p == rel,
        At::Name(n) => name == Some(n.as_os_str()),
    }
}

impl<D: DirOps> FaultDirOps<D> {
    pub fn wrap(inner: D) -> FaultDirOpsBuilder<D> {
        FaultDirOpsBuilder {
            inner,
            rules: Rules {
                enospc_nth_unlinkat: None,
                errnos: Vec::new(),
                devs: Vec::new(),
                mnt_ids: Vec::new(),
                hooks: Vec::new(),
            },
        }
    }

    /// How many times `op` was invoked, across all threads.
    pub fn calls(&self, op: Op) -> usize {
        self.counts[op as usize].load(Ordering::SeqCst)
    }

    fn bump(&self, op: Op) -> usize {
        self.counts[op as usize].fetch_add(1, Ordering::SeqCst) + 1
    }

    fn fail(&self, op: Op, rel: &Path, name: Option<&OsStr>) -> io::Result<()> {
        for (o, at, e) in &self.rules.errnos {
            if *o == op && matches(at, rel, name) {
                return Err(io::Error::from_raw_os_error(*e));
            }
        }
        Ok(())
    }

    fn fire(&self, event: Event, rel: &Path, base_abs: &Path) {
        for (e, f) in &self.rules.hooks {
            if *e == event {
                f(&HookCtx {
                    rel_path: rel.to_path_buf(),
                    base_abs: base_abs.to_path_buf(),
                    event,
                });
            }
        }
    }

    fn patch(&self, mut st: StatInfo, rel: &Path, name: Option<&OsStr>) -> StatInfo {
        for (at, dev) in &self.rules.devs {
            if matches(at, rel, name) {
                st.ident.dev = *dev;
            }
        }
        for (at, id) in &self.rules.mnt_ids {
            if matches(at, rel, name) {
                st.mnt_id = Some(*id);
            }
        }
        st
    }
}

impl<D: DirOps> FaultDirOpsBuilder<D> {
    /// The `n`-th `unlinkat` call (1-based, all threads) returns `ENOSPC`.
    #[must_use]
    pub fn enospc_on_nth_unlinkat(mut self, n: usize) -> Self {
        self.rules.enospc_nth_unlinkat = Some(n);
        self
    }

    /// Every matching call fails with `errno`.
    #[must_use]
    pub fn errno_on(mut self, op: Op, at: At, errno: i32) -> Self {
        self.rules.errnos.push((op, at, errno));
        self
    }

    #[must_use]
    pub fn report_dev(mut self, at: At, dev: u64) -> Self {
        self.rules.devs.push((at, dev));
        self
    }

    #[must_use]
    pub fn report_mnt_id(mut self, at: At, mnt_id: u64) -> Self {
        self.rules.mnt_ids.push((at, mnt_id));
        self
    }

    #[must_use]
    pub fn hook(mut self, event: Event, f: impl Fn(&HookCtx) + Send + Sync + 'static) -> Self {
        self.rules.hooks.push((event, Arc::new(f)));
        self
    }

    pub fn build(self) -> FaultDirOps<D> {
        FaultDirOps {
            inner: self.inner,
            rules: self.rules,
            counts: Default::default(),
        }
    }
}

impl<D: DirOps> DirOps for FaultDirOps<D> {
    type Dir = FaultDir<D::Dir>;
    type ReadDir = FaultReadDir<D::ReadDir>;

    fn open_root(&self, canonical_base: &Path) -> io::Result<Self::Dir> {
        self.bump(Op::OpenRoot);
        self.fail(Op::OpenRoot, Path::new(""), None)?;
        Ok(FaultDir {
            inner: self.inner.open_root(canonical_base)?,
            rel: PathBuf::new(),
            base_abs: canonical_base.to_path_buf(),
        })
    }

    fn open_dir(&self, parent: &Self::Dir, name: &OsStr) -> io::Result<Self::Dir> {
        self.bump(Op::OpenDir);
        let rel = if name == OsStr::new(".") {
            parent.rel.clone()
        } else {
            parent.rel.join(name)
        };
        self.fail(Op::OpenDir, &rel, Some(name))?;
        let dir = FaultDir {
            inner: self.inner.open_dir(&parent.inner, name)?,
            rel,
            base_abs: parent.base_abs.clone(),
        };
        self.fire(Event::AfterOpenDir, &dir.rel, &dir.base_abs);
        Ok(dir)
    }

    fn fstat(&self, d: &Self::Dir) -> io::Result<StatInfo> {
        self.bump(Op::Fstat);
        let name = d.rel.file_name();
        self.fail(Op::Fstat, &d.rel, name)?;
        Ok(self.patch(self.inner.fstat(&d.inner)?, &d.rel, name))
    }

    fn stat_at(&self, d: &Self::Dir, name: &OsStr) -> io::Result<StatInfo> {
        self.bump(Op::StatAt);
        let rel = d.rel.join(name);
        self.fail(Op::StatAt, &rel, Some(name))?;
        Ok(self.patch(self.inner.stat_at(&d.inner, name)?, &rel, Some(name)))
    }

    fn read_dir(&self, d: &Self::Dir) -> io::Result<Self::ReadDir> {
        self.bump(Op::ReadDir);
        self.fire(Event::BeforeReadDir, &d.rel, &d.base_abs);
        self.fail(Op::ReadDir, &d.rel, d.rel.file_name())?;
        Ok(FaultReadDir {
            inner: self.inner.read_dir(&d.inner)?,
        })
    }

    fn unlinkat(&self, d: &Self::Dir, name: &OsStr, dir: bool) -> io::Result<()> {
        let n = self.bump(Op::Unlinkat);
        let rel = d.rel.join(name);
        self.fire(Event::BeforeUnlinkat, &rel, &d.base_abs);
        if self.rules.enospc_nth_unlinkat == Some(n) {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        self.fail(Op::Unlinkat, &rel, Some(name))?;
        self.inner.unlinkat(&d.inner, name, dir)
    }

    fn fchmod(&self, d: &Self::Dir, mode: u32) -> io::Result<()> {
        self.bump(Op::Fchmod);
        self.fail(Op::Fchmod, &d.rel, d.rel.file_name())?;
        self.inner.fchmod(&d.inner, mode)
    }
}

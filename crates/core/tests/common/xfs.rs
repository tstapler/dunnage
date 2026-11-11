#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! A scripted read-only `Fs` with settable inodes and devices, and the other ports the explorer plan reads
//! (Story 6.6.2). `FakeFs` derives inodes from the path, so it cannot model "the same name, a new inode".

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use disk_cleanup_core::explore::mark_check::OwnState;
use disk_cleanup_core::explorer_plan::{ExplorerPlanCtx, NoCleanerView, TreeSize, TreeSizer};
use disk_cleanup_core::ports::Fs;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::testing::{FakeClock, FakeProbe};
use disk_cleanup_core::{
    AllocatedBytes, DirEntryInfo, Dirs, FileKind, Identity, MountEntry, StatInfo,
};

pub const NOW: i64 = 1_700_000_000;

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: FileKind,
    pub dev: u64,
    pub ino: u64,
    pub bytes: u64,
}

#[derive(Debug, Default)]
pub struct XFs {
    nodes: Mutex<HashMap<PathBuf, Node>>,
    next_ino: Mutex<u64>,
    pub lstats: Mutex<Vec<PathBuf>>,
    faults: Mutex<HashMap<PathBuf, i32>>,
    git: Mutex<HashMap<PathBuf, u64>>,
}

impl XFs {
    pub fn new() -> XFs {
        XFs {
            next_ino: Mutex::new(100),
            ..XFs::default()
        }
    }

    fn put(&self, p: &str, kind: FileKind, dev: u64, bytes: u64) {
        let mut n = self.next_ino.lock().unwrap();
        *n += 1;
        self.nodes.lock().unwrap().insert(
            PathBuf::from(p),
            Node {
                kind,
                dev,
                ino: *n,
                bytes,
            },
        );
    }

    /// `p` and every ancestor as directories on device 1 (ancestors only when absent).
    pub fn dir(&self, p: &str, bytes: u64) -> &XFs {
        for a in Path::new(p).ancestors().skip(1) {
            if a != Path::new("/") && !self.has(a.to_str().unwrap()) {
                self.put(a.to_str().unwrap(), FileKind::Dir, 1, 0);
            }
        }
        if !self.has("/") {
            self.put("/", FileKind::Dir, 1, 0);
        }
        self.put(p, FileKind::Dir, 1, bytes);
        self
    }

    pub fn file(&self, p: &str, bytes: u64) -> &XFs {
        self.dir(Path::new(p).parent().unwrap().to_str().unwrap(), 0);
        self.put(p, FileKind::File, 1, bytes);
        self
    }

    pub fn symlink(&self, p: &str) -> &XFs {
        self.put(p, FileKind::Symlink, 1, 0);
        self
    }

    pub fn on_dev(&self, p: &str, dev: u64) -> &XFs {
        self.nodes
            .lock()
            .unwrap()
            .get_mut(Path::new(p))
            .unwrap()
            .dev = dev;
        self
    }

    /// `lstat` of `p` fails with this errno.
    pub fn fail(&self, p: &str, errno: i32) -> &XFs {
        self.faults.lock().unwrap().insert(p.into(), errno);
        self
    }

    pub fn has(&self, p: &str) -> bool {
        self.nodes.lock().unwrap().contains_key(Path::new(p))
    }

    pub fn remove(&self, p: &str) {
        self.nodes.lock().unwrap().remove(Path::new(p));
    }

    /// The same name again with a new inode (a directory deleted and recreated).
    pub fn replace(&self, p: &str) {
        let old = self.nodes.lock().unwrap().remove(Path::new(p)).unwrap();
        self.put(p, old.kind, old.dev, old.bytes);
    }

    /// Pretends `n` `.git` entries live below `p` (what `treestat` would count).
    pub fn set_git_below(&self, p: &str, n: u64) -> &XFs {
        self.git.lock().unwrap().insert(p.into(), n);
        self
    }

    pub fn git_below(&self, p: &Path) -> u64 {
        self.git.lock().unwrap().get(p).copied().unwrap_or(0)
    }

    pub fn ino(&self, p: &str) -> u64 {
        self.nodes.lock().unwrap().get(Path::new(p)).unwrap().ino
    }
}

impl Fs for XFs {
    fn lstat(&self, path: &Path) -> io::Result<StatInfo> {
        self.lstats.lock().unwrap().push(path.to_path_buf());
        if let Some(e) = self.faults.lock().unwrap().get(path) {
            return Err(io::Error::from_raw_os_error(*e));
        }
        let n = self.nodes.lock().unwrap().get(path).cloned();
        let n = n.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        Ok(StatInfo {
            ident: Identity {
                dev: n.dev,
                ino: n.ino,
            },
            kind: n.kind,
            mode: 0o755,
            nlink: 1,
            blocks: n.bytes.div_ceil(512),
            size: n.bytes,
            mtime_secs: 0,
            mtime_nsec: 0,
            mnt_id: None,
        })
    }

    fn read_dir(&self, _path: &Path) -> io::Result<Vec<DirEntryInfo>> {
        Err(io::Error::from_raw_os_error(13))
    }

    fn read_to_string(&self, _path: &Path) -> io::Result<String> {
        Err(io::Error::from_raw_os_error(13))
    }
}

/// `treestat` as the tree's recorded bytes.
pub struct XSizer<'a>(pub &'a XFs);

impl TreeSizer for XSizer<'_> {
    fn measure(&self, path: &Path) -> io::Result<TreeSize> {
        let n = self.0.nodes.lock().unwrap().get(path).cloned();
        n.map(|n| TreeSize {
            allocated: AllocatedBytes(n.bytes),
            newest_mtime: Some(NOW - 10),
            git_repos: self.0.git_below(path),
        })
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
}

pub fn dirs() -> Dirs {
    Dirs {
        home: "/home/u".into(),
        state: "/home/u/.local/state".into(),
        cache: "/home/u/.cache".into(),
        config: "/home/u/.config".into(),
    }
}

pub fn roots(scan: &[&str]) -> RootSet {
    let canon = |p: &Path| -> io::Result<(PathBuf, Identity)> {
        Ok((
            p.to_path_buf(),
            Identity {
                dev: 1,
                ino: p.as_os_str().len() as u64 + 7,
            },
        ))
    };
    let list: Vec<PathBuf> = scan.iter().map(PathBuf::from).collect();
    RootSet::resolve(&dirs(), &[], Some(&list), Path::new("/"), &canon).unwrap()
}

pub fn own_state() -> OwnState {
    OwnState::new(&dirs(), PathBuf::from("/usr/bin/disk-cleanup"))
}

pub struct World {
    pub fs: XFs,
    pub roots: RootSet,
    pub own: OwnState,
    pub probe: FakeProbe,
    pub clock: FakeClock,
}

impl World {
    pub fn new() -> World {
        World::with_roots(&["/home/u/code"])
    }

    pub fn with_roots(scan: &[&str]) -> World {
        World {
            fs: XFs::new(),
            roots: roots(scan),
            own: own_state(),
            probe: FakeProbe::new(),
            clock: FakeClock::at(NOW),
        }
    }

    pub fn mounts(mut self, m: &[(&str, u64)]) -> World {
        self.probe = FakeProbe::new().with_mounts(
            m.iter()
                .enumerate()
                .map(|(i, (p, dev))| MountEntry {
                    mount_point: (*p).into(),
                    dev: *dev,
                    mnt_id: 100 + i as u64,
                })
                .collect(),
        );
        self
    }
}

/// Runs `f` with a context over the world.
pub fn with_ctx<R>(
    w: &World,
    cleaner: &dyn disk_cleanup_core::explorer_plan::CleanerView,
    f: impl FnOnce(&ExplorerPlanCtx<'_>) -> R,
) -> R {
    let sizer = XSizer(&w.fs);
    let ctx = ExplorerPlanCtx {
        roots: &w.roots,
        own_state: &w.own,
        fs: &w.fs,
        probe: &w.probe,
        clock: &w.clock,
        sizer: &sizer,
        extents: None,
        cleaner,
    };
    f(&ctx)
}

pub fn none() -> NoCleanerView {
    NoCleanerView
}

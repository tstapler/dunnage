//! The one tree-walk rule for sizes and recency (Story 0.1.4), shared by discovery, cache sizing and the
//! delete engine's accounting: `st_blocks * 512`, hardlinks counted once per `(dev, ino)`, bounded to
//! `root_dev`, never following symlinks, streaming (memory does not grow with directory width).
//!
//! Counting rule (Glossary, Story 2.1.2), the same one `delete_tree` applies to `Freed`: every name visited
//! counts one entry, directories and the measured directory itself included; an inode adds `blocks * 512`
//! once, directories included; `apparent` is `st_size` per name over non-directories. On a quiescent tree
//! with no links leaving it, `measure(dir)` equals the `Freed` of `delete_tree(dir)`. `allocated` counts a
//! hardlinked inode once whatever outside links it has (an upper bound), where `Freed` counts it only when
//! its last name went away.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_cleanup_core::{AllocatedBytes, ApparentBytes, FileKind, RootDev, StatInfo};

use crate::dirops::DirOps;

/// Deepest directory level the walk descends into; deeper subtrees are skipped, not followed.
pub const MAX_DEPTH: usize = 512;
/// At most this many `skipped` entries are kept; `skipped_total` still counts all of them.
pub const SKIPPED_KEPT: usize = 1024;

#[derive(Debug, Clone, Copy, Default)]
pub struct MeasureOpts {
    /// Stop as soon as an entry newer than this is seen; the result then has `stopped_early` set.
    pub stop_if_newer_than: Option<SystemTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipWhy {
    OtherDevice,
    Symlink,
    Unreadable(i32),
    TooDeep,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// Relative to the measured directory.
    pub rel: PathBuf,
    pub why: SkipWhy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeStats {
    pub allocated: AllocatedBytes,
    pub apparent: ApparentBytes,
    pub newest_mtime: Option<SystemTime>,
    /// Distinct inodes, directories included.
    pub inodes: u64,
    pub entries: u64,
    pub skipped: Vec<Skipped>,
    pub skipped_total: u64,
    pub stopped_early: bool,
    /// Entries named `.git` (a repository, or a worktree's link file) found below the measured directory.
    pub git_repos: u64,
}

fn to_system_time(secs: i64, nsec: u32) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs.unsigned_abs(), nsec)
    } else {
        UNIX_EPOCH - Duration::new(secs.unsigned_abs(), 0) + Duration::new(0, nsec)
    }
}

fn mtime_of(st: &StatInfo) -> SystemTime {
    to_system_time(st.mtime_secs, st.mtime_nsec)
}

/// One frame per open level: its handle, its entry iterator and the name that led to it.
struct Frame<D: DirOps> {
    dir: Option<D::Dir>,
    iter: D::ReadDir,
    name: Option<std::ffi::OsString>,
}

struct Walk {
    stats: TreeStats,
    /// `(dev, ino)` of entries with `nlink > 1` only.
    seen: HashSet<(u64, u64)>,
    newer_than: Option<SystemTime>,
}

impl Walk {
    fn skip(&mut self, rel: PathBuf, why: SkipWhy) {
        self.stats.skipped_total += 1;
        if self.stats.skipped.len() < SKIPPED_KEPT {
            self.stats.skipped.push(Skipped { rel, why });
        }
    }

    /// Returns true when the walk should stop.
    fn note_mtime(&mut self, st: &StatInfo) -> bool {
        let m = mtime_of(st);
        if self.stats.newest_mtime.is_none_or(|n| m > n) {
            self.stats.newest_mtime = Some(m);
        }
        if self.newer_than.is_some_and(|t| m > t) {
            self.stats.stopped_early = true;
            return true;
        }
        false
    }

    fn count_file(&mut self, st: &StatInfo) {
        // Apparent size is per name (`du --apparent-size -l`); allocated size and inodes are per inode.
        self.stats.apparent.0 += st.size;
        let first = st.nlink <= 1 || self.seen.insert((st.ident.dev, st.ident.ino));
        if first {
            self.stats.allocated.0 += st.blocks * 512;
            self.stats.inodes += 1;
        }
    }
}

/// Measures everything below `dir`. Errors on individual entries are recorded in `skipped`, never raised:
/// a partial answer is marked by `skipped` entries of kind `Unreadable`, `TooDeep` or by `stopped_early`.
pub fn measure<D: DirOps>(
    ops: &D,
    dir: &D::Dir,
    root_dev: RootDev,
    opts: MeasureOpts,
) -> TreeStats {
    let mut w = Walk {
        stats: TreeStats {
            allocated: AllocatedBytes(0),
            apparent: ApparentBytes(0),
            newest_mtime: None,
            inodes: 0,
            entries: 0,
            skipped: Vec::new(),
            skipped_total: 0,
            stopped_early: false,
            git_repos: 0,
        },
        seen: HashSet::new(),
        newer_than: opts.stop_if_newer_than,
    };

    match ops.fstat(dir) {
        Ok(st) => {
            w.stats.entries += 1;
            w.stats.allocated.0 += st.blocks * 512;
            w.stats.inodes += 1;
        }
        Err(e) => w.skip(
            PathBuf::new(),
            SkipWhy::Unreadable(e.raw_os_error().unwrap_or(libc::EIO)),
        ),
    }
    let first = match ops.read_dir(dir) {
        Ok(it) => it,
        Err(e) => {
            w.skip(
                PathBuf::new(),
                SkipWhy::Unreadable(e.raw_os_error().unwrap_or(libc::EIO)),
            );
            return w.stats;
        }
    };
    let mut stack: Vec<Frame<D>> = vec![Frame {
        dir: None,
        iter: first,
        name: None,
    }];

    let rel_of = |stack: &[Frame<D>], leaf: &std::ffi::OsStr| -> PathBuf {
        let mut p = PathBuf::new();
        for f in stack {
            if let Some(n) = &f.name {
                p.push(n);
            }
        }
        p.push(leaf);
        p
    };

    'walk: while let Some(top) = stack.last_mut() {
        let Some(item) = top.iter.next() else {
            stack.pop();
            continue;
        };
        let entry = match item {
            Ok(e) => e,
            Err(e) => {
                let rel = rel_of(&stack, std::ffi::OsStr::new(""));
                w.skip(
                    rel,
                    SkipWhy::Unreadable(e.raw_os_error().unwrap_or(libc::EIO)),
                );
                continue;
            }
        };
        // Borrow the parent handle: level 0 uses `dir`, deeper levels their own opened handle.
        let depth = stack.len();
        let parent_ref: &D::Dir = match stack.last().and_then(|f| f.dir.as_ref()) {
            Some(d) => d,
            None => dir,
        };

        let st = match ops.stat_at(parent_ref, &entry.name) {
            Ok(st) => st,
            Err(e) => {
                let rel = rel_of(&stack, &entry.name);
                w.skip(
                    rel,
                    SkipWhy::Unreadable(e.raw_os_error().unwrap_or(libc::EIO)),
                );
                continue;
            }
        };
        if st.kind == FileKind::Symlink {
            // Never followed, but the link itself is a name and an inode, as `delete_tree` counts it.
            let rel = rel_of(&stack, &entry.name);
            w.skip(rel, SkipWhy::Symlink);
        }
        if entry.name == ".git" && matches!(st.kind, FileKind::Dir | FileKind::File) {
            w.stats.git_repos += 1;
        }
        if st.kind == FileKind::Dir {
            if !st.ident.on_dev(root_dev) {
                let rel = rel_of(&stack, &entry.name);
                w.skip(rel, SkipWhy::OtherDevice);
                continue;
            }
            w.stats.entries += 1;
            w.stats.allocated.0 += st.blocks * 512;
            w.stats.inodes += 1;
            if w.note_mtime(&st) {
                break 'walk;
            }
            if depth >= MAX_DEPTH {
                let rel = rel_of(&stack, &entry.name);
                w.skip(rel, SkipWhy::TooDeep);
                continue;
            }
            let opened = ops.open_dir(parent_ref, &entry.name).and_then(|d| {
                // A swapped directory is not followed: the handle must be the entry just stat'ed.
                let again = ops.fstat(&d)?;
                if again.ident != st.ident {
                    return Err(std::io::Error::from_raw_os_error(libc::ESTALE));
                }
                let it = ops.read_dir(&d)?;
                Ok((d, it))
            });
            match opened {
                Ok((d, it)) => stack.push(Frame {
                    dir: Some(d),
                    iter: it,
                    name: Some(entry.name),
                }),
                Err(e) => {
                    let rel = rel_of(&stack, &entry.name);
                    w.skip(
                        rel,
                        SkipWhy::Unreadable(e.raw_os_error().unwrap_or(libc::EIO)),
                    );
                }
            }
            continue;
        }
        w.stats.entries += 1;
        if w.note_mtime(&st) {
            break 'walk;
        }
        w.count_file(&st);
    }
    w.stats
}

/// `treestat` as the explorer planner's size port (Task 6.6.3b): the plan and the apply's re-measure use this one
/// walk, so a figure the owner saw and the figure checked before the delete are measured the same way.
pub struct TreestatSizer<D: DirOps> {
    ops: D,
}

impl<D: DirOps> TreestatSizer<D> {
    pub fn new(ops: D) -> TreestatSizer<D> {
        TreestatSizer { ops }
    }
}

impl<D: DirOps> disk_cleanup_core::explorer_plan::TreeSizer for TreestatSizer<D> {
    fn measure(
        &self,
        path: &std::path::Path,
    ) -> std::io::Result<disk_cleanup_core::explorer_plan::TreeSize> {
        let dir = self.ops.open_root(path)?;
        let dev = self.ops.fstat(&dir)?.ident.dev;
        let stats = measure(&self.ops, &dir, RootDev(dev), MeasureOpts::default());
        Ok(disk_cleanup_core::explorer_plan::TreeSize {
            allocated: stats.allocated,
            newest_mtime: stats.newest_mtime.map(crate::mtime_probe::unix_secs),
            git_repos: stats.git_repos,
        })
    }
}

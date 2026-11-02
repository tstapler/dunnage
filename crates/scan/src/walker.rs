//! Unprivileged backend: a parallel directory walk with an incremental cache.
//!
//! Cost model per directory: one `lstat` to learn its mtime and inode. On a cache hit that is all
//! (no open, no readdir, no per-file lstat); on a miss the directory is opened with
//! `O_NOATIME`, read once, and each non-directory child is `fstatat`ed relative to the open fd.
//! Subdirectories are reported by `d_type` and never lstat'ed by their parent.
//!
//! inotify is deliberately not used: it needs one watch per directory, and this tree has more
//! directories than `fs.inotify.max_user_watches` (see docs/design.md).

use crate::cache::{self, Rec};
use crate::estimate::{EntryInfo, Kind};
use crate::index::{Backend, CollectRequest, Collected, DirInfo};
use crate::{Error, Result};
use rustix::fs::{openat, statat, AtFlags, Dir, FileType, Mode, OFlags, Stat, CWD};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LINK_SHARDS: usize = 64;
/// A directory modified this recently when listed may be modified again within the same
/// filesystem timestamp tick without its mtime changing (the "racy git" problem), so its record
/// is stored as already expired and re-listed next run.
const RACY_WINDOW_SECS: u64 = 2;

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub path: PathBuf,
    /// Ignore existing records (still writes fresh ones).
    pub rescan: bool,
    pub max_age: Duration,
}

#[derive(Debug, Default, Clone)]
pub struct WalkerBackend {
    pub cache: Option<CacheConfig>,
}

struct Work {
    path: PathBuf,
    root_dev: u64,
    is_root: bool,
}

struct Queue {
    items: Mutex<Vec<Work>>,
    cv: Condvar,
    pending: AtomicUsize,
}

struct Ctx<'a> {
    old: HashMap<PathBuf, Rec>,
    use_cache: bool,
    write_cache: bool,
    max_age_secs: u64,
    now_secs: u64,
    cross_fs: bool,
    deadline: Option<std::time::Instant>,
    stop: AtomicBool,
    errors: AtomicU64,
    noatime_denied: AtomicBool,
    links: [Mutex<HashSet<(u64, u64)>>; LINK_SHARDS],
    _req: &'a CollectRequest<'a>,
}

#[derive(Default)]
struct Out {
    dirs: Vec<(PathBuf, DirInfo)>,
    recs: Vec<(PathBuf, Rec)>,
    listed: u64,
    cached: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn entry_kind(ft: FileType) -> Kind {
    match ft {
        FileType::Directory => Kind::Dir,
        FileType::RegularFile => Kind::File,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    }
}

fn entry_info(st: &Stat, name_len: usize, first_link: bool) -> EntryInfo {
    EntryInfo {
        kind: entry_kind(FileType::from_raw_mode(st.st_mode)),
        size: st.st_size as u64,
        allocated: st.st_blocks as u64 * 512,
        name_len: name_len as u64,
        first_link,
        counts_dir_entry: true,
    }
}

impl Ctx<'_> {
    fn first_sighting(&self, dev: u64, ino: u64) -> bool {
        self.links[(ino as usize) % LINK_SHARDS]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((dev, ino))
    }

    fn expired(&self) -> bool {
        if self.stop.load(Ordering::Relaxed) {
            return true;
        }
        if self.deadline.is_some_and(|d| std::time::Instant::now() > d) {
            self.stop.store(true, Ordering::Relaxed);
            return true;
        }
        false
    }

    fn open_dir(&self, path: &Path) -> rustix::io::Result<OwnedFd> {
        let base = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        if !self.noatime_denied.load(Ordering::Relaxed) {
            match openat(CWD, path, base | OFlags::NOATIME, Mode::empty()) {
                Err(rustix::io::Errno::PERM) => self.noatime_denied.store(true, Ordering::Relaxed),
                other => return other,
            }
        }
        openat(CWD, path, base, Mode::empty())
    }

    /// Reads one directory: counters for its non-directory children plus the names of its subdirs.
    fn list(&self, path: &Path, st: &Stat) -> Option<Rec> {
        let fd = self.open_dir(path).ok()?;
        let mut dir = Dir::read_from(&fd).ok()?;
        let mut rec = Rec {
            ino: st.st_ino,
            mtime_sec: st.st_mtime,
            mtime_nsec: st.st_mtime_nsec as u32,
            listed_at: self.now_secs,
            files: crate::estimate::Stats::default(),
            has_package_json: false,
            has_cargo_toml: false,
            git_file: false,
            subdirs: Vec::new(),
        };
        let mut scratch = DirInfo::default();
        while let Some(entry) = dir.read() {
            let Ok(entry) = entry else {
                self.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            let name = entry.file_name();
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let mut ft = entry.file_type();
            let mut child = None;
            if ft == FileType::Unknown {
                let Ok(cst) = statat(&fd, name, AtFlags::SYMLINK_NOFOLLOW) else {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                ft = FileType::from_raw_mode(cst.st_mode);
                child = Some(cst);
            }
            if ft == FileType::Directory {
                rec.subdirs.push(bytes.to_vec());
                continue;
            }
            let cst = match child {
                Some(c) => c,
                None => match statat(&fd, name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(c) => c,
                    Err(_) => {
                        self.errors.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                },
            };
            let first = cst.st_nlink <= 1 || self.first_sighting(cst.st_dev, cst.st_ino);
            rec.files.add_entry(&entry_info(&cst, bytes.len(), first));
            scratch.note_file_name(bytes, ft == FileType::RegularFile);
        }
        if self
            .now_secs
            .saturating_sub(u64::try_from(st.st_mtime).unwrap_or(0))
            <= RACY_WINDOW_SECS
        {
            rec.listed_at = 0;
        }
        rec.has_package_json = scratch.has_package_json;
        rec.has_cargo_toml = scratch.has_cargo_toml;
        rec.git_file = scratch.git_file;
        Some(rec)
    }

    fn process(&self, w: &Work, out: &mut Out, children: &mut Vec<Work>) {
        let Ok(st) = statat(CWD, &w.path, AtFlags::SYMLINK_NOFOLLOW) else {
            self.errors.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if FileType::from_raw_mode(st.st_mode) != FileType::Directory {
            return;
        }
        if !w.is_root && !self.cross_fs && st.st_dev != w.root_dev {
            return;
        }
        let fresh = |r: &Rec| {
            self.use_cache
                && r.ino == st.st_ino
                && r.mtime_sec == st.st_mtime
                && r.mtime_nsec == st.st_mtime_nsec as u32
                && self.now_secs.saturating_sub(r.listed_at) < self.max_age_secs
        };
        let (rec, hit) = match self.old.get(&w.path) {
            Some(r) if fresh(r) => (r.clone(), true),
            _ => match self.list(&w.path, &st) {
                Some(r) => (r, false),
                None => {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            },
        };
        if hit {
            out.cached += 1;
        } else {
            out.listed += 1;
        }

        let name_len = w.path.file_name().map_or(0, OsStr::len);
        let mut info = DirInfo {
            mtime: st.st_mtime,
            has_package_json: rec.has_package_json,
            has_cargo_toml: rec.has_cargo_toml,
            git_file: rec.git_file,
            ..DirInfo::default()
        };
        let mut own = entry_info(&st, name_len, true);
        own.counts_dir_entry = !w.is_root;
        info.stats.add_entry(&own);
        info.stats.merge(&rec.files);
        out.dirs.push((w.path.clone(), info));

        for name in &rec.subdirs {
            children.push(Work {
                path: w.path.join(OsStr::from_bytes(name)),
                root_dev: w.root_dev,
                is_root: false,
            });
        }
        if self.write_cache {
            out.recs.push((w.path.clone(), rec));
        }
    }
}

fn worker(ctx: &Ctx<'_>, q: &Queue) -> Out {
    let mut out = Out::default();
    let mut children = Vec::new();
    loop {
        let work = {
            let mut items = q.items.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if let Some(w) = items.pop() {
                    break Some(w);
                }
                if q.pending.load(Ordering::Acquire) == 0 {
                    break None;
                }
                items = q.cv.wait(items).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let Some(w) = work else {
            q.cv.notify_all();
            return out;
        };
        children.clear();
        if !ctx.expired() {
            ctx.process(&w, &mut out, &mut children);
        }
        if !children.is_empty() {
            q.pending.fetch_add(children.len(), Ordering::AcqRel);
            q.items
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .append(&mut children);
            q.cv.notify_all();
        }
        if q.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            // Taking the lock first closes the window between a waiter's pending check and its wait.
            drop(q.items.lock().unwrap_or_else(PoisonError::into_inner));
            q.cv.notify_all();
        }
    }
}

impl Backend for WalkerBackend {
    fn name(&self) -> &'static str {
        "walker"
    }

    fn extents_exact(&self) -> bool {
        false
    }

    fn collect(&self, req: &CollectRequest<'_>) -> Result<Collected> {
        let cfg = self.cache.as_ref();
        let old = match cfg {
            Some(c) if !c.rescan => cache::load(&c.path),
            _ => HashMap::new(),
        };
        let ctx = Ctx {
            use_cache: cfg.is_some_and(|c| !c.rescan),
            write_cache: cfg.is_some(),
            max_age_secs: cfg.map_or(0, |c| c.max_age.as_secs()),
            old,
            now_secs: now_secs(),
            cross_fs: req.cross_fs,
            deadline: req.deadline,
            stop: AtomicBool::new(false),
            errors: AtomicU64::new(0),
            noatime_denied: AtomicBool::new(false),
            links: std::array::from_fn(|_| Mutex::new(HashSet::new())),
            _req: req,
        };

        let mut seeds = Vec::new();
        for root in req.roots {
            let st = statat(CWD, root, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| Error::Io {
                path: root.clone(),
                source: e.into(),
            })?;
            seeds.push(Work {
                path: root.clone(),
                root_dev: st.st_dev,
                is_root: true,
            });
        }
        let q = Queue {
            pending: AtomicUsize::new(seeds.len()),
            items: Mutex::new(seeds),
            cv: Condvar::new(),
        };
        let threads = req.threads.max(1);
        let outs: Vec<Out> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads).map(|_| s.spawn(|| worker(&ctx, &q))).collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });

        let mut collected = Collected {
            errors: ctx.errors.load(Ordering::Relaxed),
            truncated: ctx.stop.load(Ordering::Relaxed),
            ..Collected::default()
        };
        let mut visited: HashMap<PathBuf, Rec> = HashMap::new();
        for o in outs {
            collected.dirs.extend(o.dirs);
            collected.dirs_listed += o.listed;
            collected.dirs_cached += o.cached;
            visited.extend(o.recs);
        }
        if ctx.noatime_denied.load(Ordering::Relaxed) {
            collected.notes.push(
                "O_NOATIME was denied for some directory (not owned by you); its atime may update"
                    .into(),
            );
        }

        if let Some(c) = cfg {
            // Keep records outside the scanned roots; inside them only what this run saw (or, if
            // the run was cut short, whatever it did not get to).
            let mut next = ctx.old;
            if !collected.truncated {
                next.retain(|p, _| !req.roots.iter().any(|r| p.starts_with(r)));
            }
            next.extend(visited);
            if let Err(e) = cache::save(&c.path, &next) {
                collected
                    .notes
                    .push(format!("cache not saved to {}: {e}", c.path.display()));
            }
        }
        Ok(collected)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("metascope-walk-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::canonicalize(&d).unwrap()
    }

    fn age_dirs(paths: &[&Path]) {
        use rustix::fs::{utimensat, Timespec, Timestamps};
        let t = Timespec {
            tv_sec: 1_600_000_000,
            tv_nsec: 0,
        };
        for p in paths {
            let ts = Timestamps {
                last_access: t,
                last_modification: t,
            };
            utimensat(rustix::fs::CWD, *p, &ts, rustix::fs::AtFlags::empty()).unwrap();
        }
    }

    fn collect(root: &Path, cache: Option<CacheConfig>) -> Collected {
        let roots = [root.to_path_buf()];
        WalkerBackend { cache }
            .collect(&CollectRequest {
                roots: &roots,
                cross_fs: false,
                deadline: None,
                threads: 4,
            })
            .unwrap()
    }

    fn total_inodes(c: &Collected) -> u64 {
        c.dirs.iter().map(|(_, d)| d.stats.inodes).sum()
    }

    #[test]
    fn second_run_answers_from_cache_and_sees_changes() {
        let root = tmp("cache");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("c")).unwrap();
        fs::write(root.join("a/b/f1"), "x").unwrap();
        fs::write(root.join("c/f2"), "y").unwrap();
        let cache_home = tmp("cache-home");
        let cfg = CacheConfig {
            path: cache_home.join("c.bin"),
            rescan: false,
            max_age: Duration::from_secs(3600),
        };

        // Age every directory past the racy window so records are cacheable.
        age_dirs(&[&root.join("a/b"), &root.join("a"), &root.join("c"), &root]);
        let cold = collect(&root, Some(cfg.clone()));
        assert_eq!((cold.dirs_listed, cold.dirs_cached), (4, 0));
        assert_eq!(total_inodes(&cold), 6); // root, a, b, c, f1, f2
        let warm = collect(&root, Some(cfg.clone()));
        assert_eq!((warm.dirs_listed, warm.dirs_cached), (0, 4));
        assert_eq!(total_inodes(&warm), 6);

        fs::write(root.join("a/b/f3"), "z").unwrap();
        let after = collect(&root, Some(cfg));
        assert_eq!(after.dirs_listed, 1, "only a/b gained an entry");
        assert_eq!((after.dirs_cached, total_inodes(&after)), (3, 7));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&cache_home);
    }

    #[test]
    fn recently_modified_dirs_are_not_trusted() {
        let root = tmp("racy");
        fs::write(root.join("f"), "x").unwrap();
        let cache_home = tmp("racy-home");
        let cfg = CacheConfig {
            path: cache_home.join("c.bin"),
            rescan: false,
            max_age: Duration::from_secs(3600),
        };
        let _ = collect(&root, Some(cfg.clone()));
        let again = collect(&root, Some(cfg));
        assert_eq!((again.dirs_listed, again.dirs_cached), (1, 0));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&cache_home);
    }

    #[test]
    fn no_cache_always_lists_everything() {
        let root = tmp("nocache");
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("a/f"), "x").unwrap();
        let r = collect(&root, None);
        assert_eq!((r.dirs_listed, r.dirs_cached, r.errors), (2, 0, 0));
        assert_eq!(total_inodes(&r), 3);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn marker_files_set_flags_only_for_regular_files() {
        let root = tmp("flags");
        fs::write(root.join("package.json"), "{}").unwrap();
        fs::create_dir_all(root.join("sub/Cargo.toml")).unwrap(); // a dir, not a file
        let r = collect(&root, None);
        let flags = |p: &Path| {
            r.dirs
                .iter()
                .find(|(k, _)| k == p)
                .map(|(_, d)| (d.has_package_json, d.has_cargo_toml))
                .unwrap()
        };
        assert_eq!(flags(&root), (true, false));
        assert_eq!(flags(&root.join("sub")), (false, false));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn deadline_in_the_past_truncates() {
        let root = tmp("deadline");
        fs::create_dir_all(root.join("a")).unwrap();
        let roots = [root.clone()];
        let r = WalkerBackend::default()
            .collect(&CollectRequest {
                roots: &roots,
                cross_fs: false,
                deadline: std::time::Instant::now().checked_sub(Duration::from_secs(1)),
                threads: 2,
            })
            .unwrap();
        assert!(r.truncated);
        let _ = fs::remove_dir_all(&root);
    }
}

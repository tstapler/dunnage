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
use crate::index::{
    Backend, CollectRequest, Collected, DirEvent, DirInfo, StreamRequest, StreamSummary, NO_PARENT,
    PAINT_ID_BASE,
};
use crate::{Error, Result};
use rustix::fs::{openat, statat, AtFlags, Dir, FileType, Mode, OFlags, Stat, CWD};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LINK_SHARDS: usize = 64;
/// A directory modified this recently when listed may be modified again within the same
/// filesystem timestamp tick without its mtime changing (the "racy git" problem), so its record
/// is stored as already expired and re-listed next run.
const RACY_WINDOW_SECS: u64 = 2;
/// A streaming batch whose oldest event is this old is flushed by the tick thread.
const FLUSH_AGE: Duration = Duration::from_millis(50);
/// The tick thread wakes this often (half of `FLUSH_AGE`, so an event waits at most 75 ms, inside the 100 ms bound).
const TICK: Duration = Duration::from_millis(25);

/// What the walker needs from `lstat` on a directory.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct DirStat {
    pub dev: u64,
    pub ino: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    pub size: u64,
    pub blocks: u64,
    pub is_dir: bool,
}

impl DirStat {
    fn from_stat(st: &Stat) -> Self {
        Self {
            dev: dev_of(st),
            ino: st.st_ino,
            mtime_sec: st.st_mtime,
            mtime_nsec: st.st_mtime_nsec as u32,
            size: st.st_size as u64,
            blocks: st.st_blocks as u64,
            is_dir: FileType::from_raw_mode(st.st_mode) == FileType::Directory,
        }
    }
}

/// Test seams: replace the filesystem (stat and listing), the cache loader, and observe workers. Not a stable API.
#[doc(hidden)]
pub trait WalkHooks: Sync {
    fn load_cache(&self, path: &Path) -> HashMap<PathBuf, Rec> {
        cache::load(path)
    }
    /// `Some` replaces the real `lstat` (`Err` is an errno).
    fn stat_override(&self, _path: &Path) -> Option<std::result::Result<DirStat, i32>> {
        None
    }
    /// `Some` replaces the real directory listing (`Err` is an errno).
    fn list_override(&self, _path: &Path, _st: &DirStat) -> Option<std::result::Result<Rec, i32>> {
        None
    }
    /// A worker finished directory `id` (its terminal event is already in a batch).
    fn dir_finished(&self, _id: u64) {}
    fn worker_exited(&self) {}
    /// `Some` replaces the filesystem operations of the cache save, so a test can hold the write open or fail
    /// any step of it (the gate and abort wiring is then tested without racing the real write).
    fn save_io(&self) -> Option<&dyn cache::SaveIo> {
        None
    }
}

struct NoHooks;
impl WalkHooks for NoHooks {}

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
    id: u64,
    parent: u64,
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
    cancel: Option<&'a AtomicBool>,
    hooks: &'a dyn WalkHooks,
    next_id: AtomicU64,
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

/// The outcome of one directory's turn; each variant is the terminal fact for its id.
#[allow(clippy::large_enum_variant)] // transient, one per directory; boxing the record would cost an allocation each
enum Visit {
    Dir {
        info: DirInfo,
        rec: Rec,
        dev: u64,
        hit: bool,
    },
    Unreadable(i32),
    Boundary(u64),
    Gone,
}

/// Where a worker puts what it finds: `collect` accumulates, streaming publishes.
trait Emit {
    fn emit(&mut self, ctx: &Ctx<'_>, w: &Work, v: Visit);
}

impl Out {
    fn note_dir(&mut self, ctx: &Ctx<'_>, w: &Work, rec: Rec, hit: bool) -> u32 {
        if hit {
            self.cached += 1;
        } else {
            self.listed += 1;
        }
        let subdirs = rec.subdirs.len() as u32;
        if ctx.write_cache {
            self.recs.push((w.path.clone(), rec));
        }
        subdirs
    }
}

impl Emit for Out {
    fn emit(&mut self, ctx: &Ctx<'_>, w: &Work, v: Visit) {
        if let Visit::Dir { info, rec, hit, .. } = v {
            self.dirs.push((w.path.clone(), info));
            self.note_dir(ctx, w, rec, hit);
        }
    }
}

fn dir_entry_info(st: &DirStat, name_len: usize) -> EntryInfo {
    EntryInfo {
        kind: Kind::Dir,
        size: st.size,
        allocated: st.blocks * 512,
        name_len: name_len as u64,
        first_link: true,
        counts_dir_entry: true,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `st_dev` is `u64` on Linux but `i32` on Darwin; widen so device ids compare uniformly.
#[allow(clippy::unnecessary_cast)]
fn dev_of(st: &Stat) -> u64 {
    st.st_dev as u64
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
        if self.cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            self.stop.store(true, Ordering::Relaxed);
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
        // O_NOATIME exists only on Linux-family targets; elsewhere open without it.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if !self.noatime_denied.load(Ordering::Relaxed) {
            match openat(CWD, path, base | OFlags::NOATIME, Mode::empty()) {
                Err(rustix::io::Errno::PERM) => self.noatime_denied.store(true, Ordering::Relaxed),
                other => return other,
            }
        }
        openat(CWD, path, base, Mode::empty())
    }

    fn lstat(&self, path: &Path) -> std::result::Result<DirStat, i32> {
        if let Some(r) = self.hooks.stat_override(path) {
            return r;
        }
        statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW)
            .map(|st| DirStat::from_stat(&st))
            .map_err(rustix::io::Errno::raw_os_error)
    }

    /// Reads one directory: counters for its non-directory children plus the names of its subdirs.
    /// `Err` is the errno of the open or the readdir.
    fn list(&self, path: &Path, st: &DirStat) -> std::result::Result<Rec, i32> {
        let fd = self
            .open_dir(path)
            .map_err(rustix::io::Errno::raw_os_error)?;
        let mut dir = Dir::read_from(&fd).map_err(rustix::io::Errno::raw_os_error)?;
        let mut rec = Rec {
            ino: st.ino,
            mtime_sec: st.mtime_sec,
            mtime_nsec: st.mtime_nsec,
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
            let first = cst.st_nlink <= 1 || self.first_sighting(dev_of(&cst), cst.st_ino);
            rec.files.add_entry(&entry_info(&cst, bytes.len(), first));
            scratch.note_file_name(bytes, ft == FileType::RegularFile);
        }
        if self
            .now_secs
            .saturating_sub(u64::try_from(st.mtime_sec).unwrap_or(0))
            <= RACY_WINDOW_SECS
        {
            rec.listed_at = 0;
        }
        rec.has_package_json = scratch.has_package_json;
        rec.has_cargo_toml = scratch.has_cargo_toml;
        rec.git_file = scratch.git_file;
        Ok(rec)
    }

    /// Visits one directory; `children` receives its subdirectories (ids are assigned by the worker).
    fn process(&self, w: &Work, children: &mut Vec<Work>) -> Visit {
        let st = match self.lstat(&w.path) {
            Ok(st) => st,
            Err(errno) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
                return if errno == libc::ENOENT {
                    Visit::Gone
                } else {
                    Visit::Unreadable(errno)
                };
            }
        };
        if !st.is_dir {
            return Visit::Gone;
        }
        if !w.is_root && !self.cross_fs && st.dev != w.root_dev {
            return Visit::Boundary(st.dev);
        }
        let fresh = |r: &Rec| {
            self.use_cache
                && r.ino == st.ino
                && r.mtime_sec == st.mtime_sec
                && r.mtime_nsec == st.mtime_nsec
                && self.now_secs.saturating_sub(r.listed_at) < self.max_age_secs
        };
        let (rec, hit) = match self.old.get(&w.path) {
            Some(r) if fresh(r) => (r.clone(), true),
            _ => {
                let listed = self
                    .hooks
                    .list_override(&w.path, &st)
                    .unwrap_or_else(|| self.list(&w.path, &st));
                match listed {
                    Ok(r) => (r, false),
                    Err(errno) => {
                        self.errors.fetch_add(1, Ordering::Relaxed);
                        return Visit::Unreadable(errno);
                    }
                }
            }
        };

        let name_len = w.path.file_name().map_or(0, OsStr::len);
        let mut info = DirInfo {
            mtime: st.mtime_sec,
            has_package_json: rec.has_package_json,
            has_cargo_toml: rec.has_cargo_toml,
            git_file: rec.git_file,
            ..DirInfo::default()
        };
        let mut own = dir_entry_info(&st, name_len);
        own.counts_dir_entry = !w.is_root;
        info.stats.add_entry(&own);
        info.stats.merge(&rec.files);

        for name in &rec.subdirs {
            children.push(Work {
                path: w.path.join(OsStr::from_bytes(name)),
                root_dev: w.root_dev,
                is_root: false,
                id: 0,
                parent: w.id,
            });
        }
        Visit::Dir {
            info,
            rec,
            dev: st.dev,
            hit,
        }
    }
}

fn worker<E: Emit>(ctx: &Ctx<'_>, q: &Queue, mut out: E) -> E {
    let mut children = Vec::new();
    loop {
        let work = {
            let mut items = q.items.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if let Some(w) = items.pop() {
                    break Some(w);
                }
                if q.pending.load(Ordering::Acquire) == 0 || ctx.stop.load(Ordering::Relaxed) {
                    break None;
                }
                items = q.cv.wait(items).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let Some(w) = work else {
            q.cv.notify_all();
            ctx.hooks.worker_exited();
            return out;
        };
        children.clear();
        if !ctx.expired() {
            let visit = ctx.process(&w, &mut children);
            if !children.is_empty() {
                let base = ctx
                    .next_id
                    .fetch_add(children.len() as u64, Ordering::Relaxed);
                for (i, c) in children.iter_mut().enumerate() {
                    c.id = base + i as u64;
                }
            }
            out.emit(ctx, &w, visit);
            ctx.hooks.dir_finished(w.id);
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
            cancel: None,
            hooks: &NoHooks,
            next_id: AtomicU64::new(req.roots.len() as u64),
            errors: AtomicU64::new(0),
            noatime_denied: AtomicBool::new(false),
            links: std::array::from_fn(|_| Mutex::new(HashSet::new())),
            _req: req,
        };

        let mut seeds = Vec::new();
        for (i, root) in req.roots.iter().enumerate() {
            let st = ctx.lstat(root).map_err(|errno| Error::Io {
                path: root.clone(),
                source: io::Error::from_raw_os_error(errno),
            })?;
            seeds.push(Work {
                path: root.clone(),
                root_dev: st.dev,
                is_root: true,
                id: i as u64,
                parent: NO_PARENT,
            });
        }
        let q = Queue {
            pending: AtomicUsize::new(seeds.len()),
            items: Mutex::new(seeds),
            cv: Condvar::new(),
        };
        let threads = req.threads.max(1);
        let outs: Vec<Out> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|_| s.spawn(|| worker(&ctx, &q, Out::default())))
                .collect();
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

    fn collect_streaming(
        &self,
        req: &StreamRequest<'_>,
        sink: &(dyn Fn(Vec<DirEvent>) + Sync),
    ) -> io::Result<StreamSummary> {
        self.stream(req, sink, &NoHooks)
    }
}

/// The per-worker batch of events not yet handed to the sink.
#[derive(Default)]
struct Batch {
    events: Vec<DirEvent>,
    /// When the oldest event in `events` was created.
    since: Option<Instant>,
}

struct Shared<'a> {
    batches: Vec<Mutex<Batch>>,
    sink: &'a (dyn Fn(Vec<DirEvent>) + Sync),
    batch_max: usize,
}

impl Shared<'_> {
    fn push(&self, slot: usize, ev: DirEvent) {
        let full = {
            let mut b = self.batches[slot]
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if b.events.is_empty() {
                b.since = Some(Instant::now());
                b.events.reserve(self.batch_max);
            }
            b.events.push(ev);
            (b.events.len() >= self.batch_max).then(|| std::mem::take(&mut b.events))
        };
        if let Some(events) = full {
            (self.sink)(events);
        }
    }

    /// Hands the sink every batch older than `min_age` (all of them for `ZERO`), in chunks of at most `batch_max`.
    fn flush(&self, min_age: Duration) {
        let mut due: Vec<DirEvent> = Vec::new();
        for b in &self.batches {
            let mut b = b.lock().unwrap_or_else(PoisonError::into_inner);
            if b.since.is_some_and(|t| t.elapsed() >= min_age) {
                due.append(&mut b.events);
                b.since = None;
            }
        }
        while !due.is_empty() {
            let rest = due.split_off(due.len().min(self.batch_max));
            (self.sink)(std::mem::replace(&mut due, rest));
        }
    }
}

struct StreamEmit<'a, 'b> {
    out: Out,
    shared: &'a Shared<'b>,
    slot: usize,
}

fn event_name(w: &Work) -> OsString {
    if w.is_root {
        w.path.as_os_str().to_owned()
    } else {
        w.path.file_name().map(OsStr::to_owned).unwrap_or_default()
    }
}

impl Emit for StreamEmit<'_, '_> {
    fn emit(&mut self, ctx: &Ctx<'_>, w: &Work, v: Visit) {
        let (id, parent, name) = (w.id, w.parent, event_name(w));
        let ev = match v {
            Visit::Dir {
                info,
                rec,
                dev,
                hit,
            } => DirEvent::Dir {
                id,
                parent,
                name,
                dev,
                info,
                subdirs: self.out.note_dir(ctx, w, rec, hit),
                from_cache: hit,
                painted: false,
            },
            Visit::Unreadable(errno) => DirEvent::Unreadable {
                id,
                parent,
                name,
                errno,
            },
            Visit::Boundary(dev) => DirEvent::Boundary {
                id,
                parent,
                name,
                dev,
            },
            Visit::Gone => DirEvent::Gone { id, parent, name },
        };
        self.shared.push(self.slot, ev);
    }
}

/// Wakes at `TICK`: flushes aged batches and turns the cancel token into `stop` plus a wake-up of idle workers.
fn tick(ctx: &Ctx<'_>, q: &Queue, shared: &Shared<'_>, done: &(Mutex<bool>, Condvar)) {
    let (flag, cv) = done;
    loop {
        {
            let guard = flag.lock().unwrap_or_else(PoisonError::into_inner);
            if *guard {
                return;
            }
            let (guard, _) = cv
                .wait_timeout(guard, TICK)
                .unwrap_or_else(PoisonError::into_inner);
            if *guard {
                return;
            }
        }
        if ctx.cancel.is_some_and(|c| c.load(Ordering::Relaxed))
            && !ctx.stop.swap(true, Ordering::Relaxed)
        {
            drop(q.items.lock().unwrap_or_else(PoisonError::into_inner));
            q.cv.notify_all();
        }
        shared.flush(FLUSH_AGE);
    }
}

/// A provisional `Dir` event per cache record reachable from the roots, ids from `PAINT_ID_BASE`.
fn paint(ctx: &Ctx<'_>, req: &StreamRequest<'_>, shared: &Shared<'_>) {
    let mut next = PAINT_ID_BASE;
    let mut todo: VecDeque<(PathBuf, u64, bool)> = req
        .collect
        .roots
        .iter()
        .map(|r| (r.clone(), NO_PARENT, true))
        .collect();
    while let Some((path, parent, is_root)) = todo.pop_front() {
        let Some(rec) = ctx.old.get(&path) else {
            continue;
        };
        let id = next;
        next += 1;
        let name_len = path.file_name().map_or(0, OsStr::len);
        let mut info = DirInfo {
            mtime: rec.mtime_sec,
            has_package_json: rec.has_package_json,
            has_cargo_toml: rec.has_cargo_toml,
            git_file: rec.git_file,
            ..DirInfo::default()
        };
        // The record keeps no size for the directory's own inode, so the preview counts the inode only.
        info.stats.add_entry(&EntryInfo {
            kind: Kind::Dir,
            size: 0,
            allocated: 0,
            name_len: name_len as u64,
            first_link: true,
            counts_dir_entry: !is_root,
        });
        info.stats.merge(&rec.files);
        for sub in &rec.subdirs {
            todo.push_back((path.join(OsStr::from_bytes(sub)), id, false));
        }
        shared.push(
            0,
            DirEvent::Dir {
                id,
                parent,
                name: if is_root {
                    path.as_os_str().to_owned()
                } else {
                    path.file_name().map(OsStr::to_owned).unwrap_or_default()
                },
                dev: 0,
                info,
                subdirs: rec.subdirs.len() as u32,
                from_cache: true,
                painted: true,
            },
        );
        if ctx.cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return;
        }
    }
}

impl WalkerBackend {
    /// `collect_streaming` with the filesystem and the cache loader replaceable (tests only).
    #[doc(hidden)]
    pub fn stream(
        &self,
        req: &StreamRequest<'_>,
        sink: &(dyn Fn(Vec<DirEvent>) + Sync),
        hooks: &dyn WalkHooks,
    ) -> io::Result<StreamSummary> {
        if req.cancel.load(Ordering::Relaxed) {
            return Ok(StreamSummary {
                truncated: true,
                ..StreamSummary::default()
            });
        }
        let cfg = self.cache.as_ref();
        let old = match cfg {
            Some(c) if !c.rescan => hooks.load_cache(&c.path),
            _ => HashMap::new(),
        };
        let roots = req.collect.roots;
        let ctx = Ctx {
            use_cache: cfg.is_some_and(|c| !c.rescan),
            write_cache: cfg.is_some(),
            max_age_secs: cfg.map_or(0, |c| c.max_age.as_secs()),
            old,
            now_secs: now_secs(),
            cross_fs: req.collect.cross_fs,
            deadline: req.collect.deadline,
            stop: AtomicBool::new(false),
            cancel: Some(&req.cancel),
            hooks,
            next_id: AtomicU64::new(roots.len() as u64),
            errors: AtomicU64::new(0),
            noatime_denied: AtomicBool::new(false),
            links: std::array::from_fn(|_| Mutex::new(HashSet::new())),
            _req: &req.collect,
        };
        let mut seeds = Vec::new();
        for (i, root) in roots.iter().enumerate() {
            let st = ctx.lstat(root).map_err(io::Error::from_raw_os_error)?;
            seeds.push(Work {
                path: root.clone(),
                root_dev: st.dev,
                is_root: true,
                id: i as u64,
                parent: NO_PARENT,
            });
        }
        let threads = req.collect.threads.max(1);
        let shared = Shared {
            batches: (0..threads).map(|_| Mutex::new(Batch::default())).collect(),
            sink,
            batch_max: req.batch_max.max(1),
        };
        if req.paint_from_cache {
            paint(&ctx, req, &shared);
            // Painted events sit in slot 0's batch; flush so live workers' batches cannot overtake the remainder.
            shared.flush(Duration::ZERO);
        }
        let q = Queue {
            pending: AtomicUsize::new(seeds.len()),
            items: Mutex::new(seeds),
            cv: Condvar::new(),
        };
        let done = (Mutex::new(false), Condvar::new());
        let outs: Vec<Out> = std::thread::scope(|s| {
            let ticker = s.spawn(|| tick(&ctx, &q, &shared, &done));
            let handles: Vec<_> = (0..threads)
                .map(|slot| {
                    let (ctx, q, shared) = (&ctx, &q, &shared);
                    s.spawn(move || {
                        let emit = StreamEmit {
                            out: Out::default(),
                            shared,
                            slot,
                        };
                        worker(ctx, q, emit).out
                    })
                })
                .collect();
            let outs = handles.into_iter().filter_map(|h| h.join().ok()).collect();
            // (1) the workers are done; (2) stop and join the tick thread before the scope ends.
            *done.0.lock().unwrap_or_else(PoisonError::into_inner) = true;
            done.1.notify_all();
            let _ = ticker.join();
            outs
        });
        // (3) nothing stays in a worker batch.
        shared.flush(Duration::ZERO);

        let mut summary = StreamSummary {
            errors: ctx.errors.load(Ordering::Relaxed),
            truncated: ctx.stop.load(Ordering::Relaxed),
            ..StreamSummary::default()
        };
        let mut visited: HashMap<PathBuf, Rec> = HashMap::new();
        for o in outs {
            summary.dirs_listed += o.listed;
            summary.dirs_cached += o.cached;
            visited.extend(o.recs);
        }
        if ctx.noatime_denied.load(Ordering::Relaxed) {
            summary.notes.push(
                "O_NOATIME was denied for some directory (not owned by you); its atime may update"
                    .into(),
            );
        }
        // (4) the cache is saved only while the gate is set, from the visited records plus the old
        // records of directories this run did not reach.
        if let Some(c) = cfg.filter(|_| req.save.load(Ordering::Relaxed)) {
            let refused = req.save_guard.as_ref().and_then(|g| g(&c.path).err());
            if let Some(why) = refused {
                summary.notes.push(format!("cache not saved: {why}"));
                return Ok(summary);
            }
            let mut next = ctx.old;
            if !summary.truncated {
                next.retain(|p, _| !roots.iter().any(|r| p.starts_with(r)));
            }
            next.extend(visited);
            if let Err(e) = save_gated(&c.path, &next, &req.save, hooks.save_io()) {
                summary
                    .notes
                    .push(format!("cache not saved to {}: {e}", c.path.display()));
            }
        }
        Ok(summary)
    }
}

/// Saves the cache, aborting the write when `gate` is cleared meanwhile (`save_atomic` polls its `abort`).
fn save_gated(
    path: &Path,
    map: &HashMap<PathBuf, Rec>,
    gate: &AtomicBool,
    io: Option<&dyn cache::SaveIo>,
) -> std::result::Result<(), cache::SaveError> {
    let abort = AtomicBool::new(false);
    let finished = AtomicBool::new(false);
    std::thread::scope(|s| {
        let watcher = s.spawn(|| {
            while !finished.load(Ordering::Relaxed) {
                if !gate.load(Ordering::Relaxed) {
                    abort.store(true, Ordering::Relaxed);
                    return;
                }
                std::thread::park_timeout(Duration::from_millis(2));
            }
        });
        let result = match io {
            Some(io) => cache::save_atomic(path, map, io, &abort),
            None => cache::save_with_abort(path, map, &abort),
        };
        finished.store(true, Ordering::Relaxed);
        watcher.thread().unpark();
        result
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("dunnage-scan-walk-{name}-{}", std::process::id()));
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

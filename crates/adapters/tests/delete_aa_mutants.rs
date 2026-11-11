#![allow(
    non_snake_case,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::type_complexity,
    clippy::needless_pass_by_value
)]
//! Mutation-killing tests for `delete.rs` and `dirops.rs`, Task 2.1.5b. Written by agent T against the
//! surviving `cargo mutants` list (`ci/mutants.d/engine.survivors.txt`), not against the engine's intent.
//!
//! Every filesystem effect is inside a `tempfile` directory and every mutation goes through
//! `RealDirOps::confined_to(<tempdir>)`. `Spy` wraps `RealDirOps` to inject races and count calls. A
//! watchdog in `Spy` sets the `AbortFlag` after `WATCHDOG_OPS` operations, so a mutant that loops forever
//! makes the engine return `Err(Refused)` and the test fail, instead of hanging until a timeout; a 20 s
//! wall-clock timer does the same for a loop that performs no operation.
//!
//! This file is named to sort before `delete_adversarial.rs`: `cargo test` runs integration test binaries in
//! name order and stops at the first failure, so a mutant that makes a later suite spin forever is caught
//! here first.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use disk_cleanup_adapters::dirops::{
    is_scope_violation, RealDir, RealReadDir, DELETE_BATCH, MAX_OPEN_LEVELS,
};
use disk_cleanup_adapters::{delete_tree, open_parent, open_verified_root, DirOps, RealDirOps};
use disk_cleanup_core::{
    AbortFlag, AllowedTarget, ApplyToken, DeleteError, DeleteFailure, DeletePolicy, DirEntryInfo,
    FileKind, Freed, Identity, NamePattern, RootDev, StatInfo,
};

const WATCHDOG_OPS: usize = 60_000;

// ------------------------------------------------------------------------------------------------
// Spy
// ------------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum StatAct {
    Real,
    Err(i32),
    Kind(FileKind),
}

#[derive(Clone, Copy)]
enum UnlinkAct {
    Real,
    /// Fail without touching the filesystem.
    Err(i32),
    /// Really unlink, then report this errno (the entry vanished under the caller).
    ThenErr(i32),
}

#[derive(Clone, Copy)]
enum OpenAct {
    Real,
    Err(i32),
}

/// `(name, number of earlier calls with that name)`.
type Hook<A> = Box<dyn Fn(&OsStr, usize) -> A + Send + Sync>;

type Registry = Arc<Mutex<HashMap<usize, Identity>>>;

struct SDir {
    inner: RealDir,
    live: Arc<AtomicUsize>,
    id: usize,
    registry: Registry,
}

impl Drop for SDir {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
        self.registry.lock().unwrap().remove(&self.id);
    }
}

struct SRead {
    inner: RealReadDir,
    as_file: Vec<OsString>,
}

impl Iterator for SRead {
    type Item = io::Result<DirEntryInfo>;
    fn next(&mut self) -> Option<Self::Item> {
        let mut item = self.inner.next()?;
        if let Ok(e) = &mut item {
            if self.as_file.contains(&e.name) {
                e.kind = FileKind::File;
            }
        }
        Some(item)
    }
}

struct Spy {
    inner: RealDirOps,
    abort: Arc<AbortFlag>,
    stat_at: Hook<StatAct>,
    unlink: Box<dyn Fn(&OsStr, bool, usize) -> UnlinkAct + Send + Sync>,
    open_dir: Hook<OpenAct>,
    as_file: Vec<OsString>,
    on_unlink: Box<dyn Fn(usize) + Send + Sync>,
    seen: Mutex<HashMap<(&'static str, OsString), usize>>,
    total: AtomicUsize,
    reads: AtomicUsize,
    unlinks: AtomicUsize,
    fchmods: Mutex<Vec<u32>>,
    live: Arc<AtomicUsize>,
    max_live: AtomicUsize,
    /// `max_live` as it stood at the first `unlinkat`, i.e. the descent phase only.
    max_live_at_first_unlink: AtomicUsize,
    registry: Registry,
    next_id: AtomicUsize,
    /// Most handles open at once at any `unlinkat` of a directory.
    max_open_at_dir_unlink: AtomicUsize,
    /// Identities of the directory handles that were open at the first `unlinkat`.
    open_at_first_unlink: Mutex<Option<Vec<Identity>>>,
}

impl Spy {
    fn new(base: &Path) -> Spy {
        // Wall-clock watchdog for a loop that never reaches the engine's per-operation `tick`, but does
        // reach its `check_abort` at the top of the run loop.
        let abort = Arc::new(AbortFlag::new());
        let watchdog = abort.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(20));
            watchdog.set();
        });
        Spy {
            inner: RealDirOps::confined_to(base),
            abort,
            stat_at: Box::new(|_, _| StatAct::Real),
            unlink: Box::new(|_, _, _| UnlinkAct::Real),
            open_dir: Box::new(|_, _| OpenAct::Real),
            as_file: Vec::new(),
            on_unlink: Box::new(|_| {}),
            seen: Mutex::new(HashMap::new()),
            total: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
            unlinks: AtomicUsize::new(0),
            fchmods: Mutex::new(Vec::new()),
            live: Arc::new(AtomicUsize::new(0)),
            max_live: AtomicUsize::new(0),
            max_live_at_first_unlink: AtomicUsize::new(usize::MAX),
            registry: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicUsize::new(0),
            max_open_at_dir_unlink: AtomicUsize::new(0),
            open_at_first_unlink: Mutex::new(None),
        }
    }

    fn tick(&self) {
        if self.total.fetch_add(1, Ordering::SeqCst) > WATCHDOG_OPS {
            self.abort.set();
        }
    }

    fn nth(&self, op: &'static str, name: &OsStr) -> usize {
        let mut m = self.seen.lock().unwrap();
        let c = m.entry((op, name.to_os_string())).or_insert(0);
        let n = *c;
        *c += 1;
        n
    }

    fn wrap(&self, inner: RealDir) -> SDir {
        let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_live.fetch_max(now, Ordering::SeqCst);
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let ident = self.inner.fstat(&inner).unwrap().ident;
        self.registry.lock().unwrap().insert(id, ident);
        SDir {
            inner,
            live: self.live.clone(),
            id,
            registry: self.registry.clone(),
        }
    }
}

fn errno(n: i32) -> io::Error {
    io::Error::from_raw_os_error(n)
}

impl DirOps for Spy {
    type Dir = SDir;
    type ReadDir = SRead;

    fn open_root(&self, p: &Path) -> io::Result<SDir> {
        self.tick();
        Ok(self.wrap(self.inner.open_root(p)?))
    }

    fn open_dir(&self, parent: &SDir, name: &OsStr) -> io::Result<SDir> {
        self.tick();
        let n = self.nth("open_dir", name);
        if let OpenAct::Err(e) = (self.open_dir)(name, n) {
            return Err(errno(e));
        }
        Ok(self.wrap(self.inner.open_dir(&parent.inner, name)?))
    }

    fn fstat(&self, d: &SDir) -> io::Result<StatInfo> {
        self.tick();
        self.inner.fstat(&d.inner)
    }

    fn stat_at(&self, d: &SDir, name: &OsStr) -> io::Result<StatInfo> {
        self.tick();
        let n = self.nth("stat_at", name);
        match (self.stat_at)(name, n) {
            StatAct::Real => self.inner.stat_at(&d.inner, name),
            StatAct::Err(e) => Err(errno(e)),
            StatAct::Kind(k) => {
                let mut st = self.inner.stat_at(&d.inner, name)?;
                st.kind = k;
                Ok(st)
            }
        }
    }

    fn read_dir(&self, d: &SDir) -> io::Result<SRead> {
        self.tick();
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(SRead {
            inner: self.inner.read_dir(&d.inner)?,
            as_file: self.as_file.clone(),
        })
    }

    fn unlinkat(&self, d: &SDir, name: &OsStr, dir: bool) -> io::Result<()> {
        self.tick();
        let k = self.unlinks.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.max_live_at_first_unlink.compare_exchange(
            usize::MAX,
            self.max_live.load(Ordering::SeqCst),
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        if dir {
            let open = self.registry.lock().unwrap().len();
            self.max_open_at_dir_unlink
                .fetch_max(open, Ordering::SeqCst);
        }
        if k == 1 {
            let idents = self.registry.lock().unwrap().values().copied().collect();
            *self.open_at_first_unlink.lock().unwrap() = Some(idents);
        }
        (self.on_unlink)(k);
        let n = self.nth("unlinkat", name);
        match (self.unlink)(name, dir, n) {
            UnlinkAct::Real => self.inner.unlinkat(&d.inner, name, dir),
            UnlinkAct::Err(e) => Err(errno(e)),
            UnlinkAct::ThenErr(e) => {
                self.inner.unlinkat(&d.inner, name, dir)?;
                Err(errno(e))
            }
        }
    }

    fn fchmod(&self, d: &SDir, mode: u32) -> io::Result<()> {
        self.tick();
        self.fchmods.lock().unwrap().push(mode);
        self.inner.fchmod(&d.inner, mode)
    }
}

// ------------------------------------------------------------------------------------------------
// Fixture
// ------------------------------------------------------------------------------------------------

/// `<tmp>/home/code/app/{package.json,node_modules}`; the tempdir is the confinement root.
struct Fx {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    nm: PathBuf,
}

fn ident(p: &Path) -> Identity {
    Identity::of(&fs::symlink_metadata(p).unwrap())
}

fn write(p: &Path, s: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, s).unwrap();
}

impl Fx {
    fn new() -> Fx {
        let tmp = tempfile::Builder::new()
            .prefix("dc-mut-")
            .tempdir()
            .unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let std_tmp = fs::canonicalize(std::env::temp_dir()).unwrap();
        assert!(
            base.starts_with(&std_tmp) && base != std_tmp,
            "{}",
            base.display()
        );
        let home = base.join("home");
        let app = home.join("code/app");
        let nm = app.join("node_modules");
        fs::create_dir_all(&nm).unwrap();
        write(&app.join("package.json"), "{}");
        Fx {
            _tmp: tmp,
            base,
            home,
            nm,
        }
    }

    fn policy(&self) -> DeletePolicy {
        DeletePolicy {
            root_dev: RootDev(fs::metadata(&self.base).unwrap().dev()),
            root_mnt: None,
            home: self.home.clone(),
            home_id: ident(&self.home),
            scan_roots: vec![self.home.join("code")],
            allowed: AllowedTarget::Within {
                root: self.home.join("code"),
                name: NamePattern::NodeModules,
            },
            target: self.nm.clone(),
            require_sibling_any: vec![OsString::from("package.json")],
            max_depth: 256,
            restore_owner_write: false,
        }
    }

    fn spy(&self) -> Spy {
        Spy::new(&self.base)
    }

    fn run(&self, spy: &Spy, pol: &DeletePolicy) -> Result<Freed, DeleteFailure> {
        self.run_named(spy, pol, "node_modules", ident(&self.nm))
    }

    fn run_named(
        &self,
        spy: &Spy,
        pol: &DeletePolicy,
        name: &str,
        expect: Identity,
    ) -> Result<Freed, DeleteFailure> {
        run_in(&self.base, spy, pol, name, expect)
    }

    /// Runs the delete on its own thread and fails the test after 30 s, so a mutant that spins inside the
    /// engine (where the `Spy` watchdog never sees an operation) cannot hang the suite.
    fn run_bounded(&self, spy: Spy, pol: DeletePolicy) -> (Result<Freed, DeleteFailure>, Spy) {
        let base = self.base.clone();
        let expect = ident(&self.nm);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = run_in(&base, &spy, &pol, "node_modules", expect);
            let _ = tx.send((r, spy));
        });
        rx.recv_timeout(std::time::Duration::from_secs(30))
            .expect("the engine did not return within 30 s (a loop that never ends)")
    }

    fn files(&self, n: usize) -> Vec<PathBuf> {
        (0..n)
            .map(|i| {
                let p = self.nm.join(format!("f{i:04}"));
                write(&p, "x");
                p
            })
            .collect()
    }
}

fn run_in(
    base: &Path,
    spy: &Spy,
    pol: &DeletePolicy,
    name: &str,
    expect: Identity,
) -> Result<Freed, DeleteFailure> {
    let root = open_verified_root(spy, base, ident(base)).unwrap();
    let parent = open_parent(spy, &root, Path::new("home/code/app")).unwrap();
    delete_tree(
        spy,
        &parent,
        OsStr::new(name),
        expect,
        pol,
        &spy.abort,
        &ApplyToken::for_tests(),
    )
}

fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

fn err_of(r: Result<Freed, DeleteFailure>) -> DeleteFailure {
    r.expect_err("expected the delete to fail")
}

// ------------------------------------------------------------------------------------------------
// classify / abort classes (I4, I7)
// ------------------------------------------------------------------------------------------------

#[test]
fn delete_tree_should_return_Ok_with_nothing_freed_When_the_target_is_already_gone() {
    let fx = Fx::new();
    let id = ident(&fx.nm);
    fs::remove_dir(&fx.nm).unwrap();
    let spy = fx.spy();
    let r = fx.run_named(&spy, &fx.policy(), "node_modules", id);
    assert_eq!(r, Ok(Freed::default()));
}

#[test]
fn delete_tree_should_treat_ENOENT_from_unlinkat_as_removed_When_the_entry_vanishes_under_it() {
    let fx = Fx::new();
    fx.files(4);
    let mut spy = fx.spy();
    spy.unlink = Box::new(|n, dir, _| {
        if !dir && n == OsStr::new("f0002") {
            UnlinkAct::ThenErr(libc::ENOENT)
        } else {
            UnlinkAct::Real
        }
    });
    let freed = fx.run(&spy, &fx.policy()).expect("ENOENT is success (I7)");
    assert!(!exists(&fx.nm));
    // three files counted, the vanished one is not, plus the directory itself
    assert_eq!(freed.entries_removed, 4);
}

#[test]
fn delete_tree_should_treat_ENOENT_from_stat_at_of_a_leaf_as_vanished_and_rescan_When_listed_entry_disappears(
) {
    let fx = Fx::new();
    fx.files(1);
    let mut spy = fx.spy();
    spy.stat_at = Box::new(|n, k| {
        if n == OsStr::new("f0000") && k == 0 {
            StatAct::Err(libc::ENOENT)
        } else {
            StatAct::Real
        }
    });
    let r = fx.run(&spy, &fx.policy());
    assert!(r.is_ok(), "{r:?}");
    assert!(!exists(&fx.nm));
}

fn abort_class(errno_value: i32) {
    let fx = Fx::new();
    fx.files(8);
    let mut spy = fx.spy();
    // fail on the third unlinkat overall
    spy.unlink = Box::new(move |_, _, _| {
        if NTH.fetch_add(1, Ordering::SeqCst) == 2 {
            UnlinkAct::Err(errno_value)
        } else {
            UnlinkAct::Real
        }
    });
    NTH.store(0, Ordering::SeqCst);
    let f = err_of(fx.run(&spy, &fx.policy()));
    assert_eq!(f.error, DeleteError::NoSpace, "errno {errno_value}");
    assert!(spy.abort.is_set(), "an abort-class errno sets the flag");
    assert_eq!(
        f.freed.entries_removed, 2,
        "stops at once, partial Freed kept"
    );
    let left = fs::read_dir(&fx.nm).unwrap().count();
    assert_eq!(left, 6, "no unlink after the abort-class failure");
}

static NTH: AtomicUsize = AtomicUsize::new(0);
static NTH_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn unlinkat_ENOSPC_EDQUOT_EIO_should_each_abort_the_whole_call_When_one_unlink_fails() {
    let _g = NTH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for e in [libc::ENOSPC, libc::EDQUOT, libc::EIO] {
        abort_class(e);
    }
}

#[test]
fn unlinkat_EACCES_should_not_abort_and_siblings_should_be_removed_When_one_unlink_fails() {
    let fx = Fx::new();
    fx.files(6);
    let mut spy = fx.spy();
    spy.unlink = Box::new(|n, dir, _| {
        if !dir && n == OsStr::new("f0003") {
            UnlinkAct::Err(libc::EACCES)
        } else {
            UnlinkAct::Real
        }
    });
    let f = err_of(fx.run(&spy, &fx.policy()));
    assert_eq!(f.error, DeleteError::Io(libc::EACCES));
    assert!(!spy.abort.is_set(), "a per-entry error is not an abort");
    assert_eq!(f.freed.entries_removed, 5);
    assert!(exists(&fx.nm.join("f0003")));
    assert_eq!(fs::read_dir(&fx.nm).unwrap().count(), 1);
}

#[test]
fn read_dir_ENOSPC_should_abort_with_NoSpace_When_the_listing_itself_fails_with_an_abort_errno() {
    let fx = Fx::new();
    fx.files(2);
    let mut spy = fx.spy();
    spy.stat_at = Box::new(|n, _| {
        if n == OsStr::new("f0000") {
            StatAct::Err(libc::EIO)
        } else {
            StatAct::Real
        }
    });
    let f = err_of(fx.run(&spy, &fx.policy()));
    assert_eq!(f.error, DeleteError::NoSpace);
    assert!(spy.abort.is_set());
}

#[test]
fn check_abort_should_stop_the_walk_with_Refused_When_the_flag_is_set_mid_run() {
    let fx = Fx::new();
    fx.files(10);
    let mut spy = fx.spy();
    let flag = spy.abort.clone();
    spy.on_unlink = Box::new(move |k| {
        if k == 3 {
            flag.set();
        }
    });
    let f = err_of(fx.run(&spy, &fx.policy()));
    assert_eq!(f.error, DeleteError::Refused);
    assert!(fx.nm.exists(), "the target directory is not removed");
    assert!(
        fs::read_dir(&fx.nm).unwrap().count() >= 7,
        "nothing is unlinked after the flag is seen"
    );
}

// ------------------------------------------------------------------------------------------------
// open_frame: identity and fchmod
// ------------------------------------------------------------------------------------------------

fn mode_tree(fx: &Fx) {
    let ro = fx.nm.join("ro");
    let rw = fx.nm.join("rw");
    write(&ro.join("a"), "x");
    write(&rw.join("b"), "x");
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
    fs::set_permissions(&rw, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn open_frame_should_fchmod_only_directories_missing_owner_rwx_When_restore_owner_write_is_set() {
    let fx = Fx::new();
    mode_tree(&fx);
    let spy = fx.spy();
    let mut pol = fx.policy();
    pol.restore_owner_write = true;
    let r = fx.run(&spy, &pol);
    assert!(r.is_ok(), "{r:?}");
    assert!(!exists(&fx.nm));
    assert_eq!(
        *spy.fchmods.lock().unwrap(),
        vec![0o755],
        "one fchmod, for the 0555 directory only, restoring owner rwx"
    );
}

#[test]
fn open_frame_should_not_fchmod_When_restore_owner_write_is_unset() {
    let fx = Fx::new();
    mode_tree(&fx);
    let spy = fx.spy();
    let _ = fx.run(&spy, &fx.policy());
    assert!(spy.fchmods.lock().unwrap().is_empty());
    // test cleanup: give the owner rwx back so the tempdir can be removed
    let _ = fs::set_permissions(fx.nm.join("ro"), fs::Permissions::from_mode(0o755));
}

// ------------------------------------------------------------------------------------------------
// handle window (MAX_OPEN_LEVELS)
// ------------------------------------------------------------------------------------------------

#[test]
fn open_frame_should_keep_exactly_MAX_OPEN_LEVELS_handles_open_When_descending_a_deep_chain() {
    let fx = Fx::new();
    let mut p = fx.nm.clone();
    for i in 0..(MAX_OPEN_LEVELS * 3) {
        p = p.join(format!("d{i}"));
    }
    fs::create_dir_all(&p).unwrap();
    // frame k of the engine is the directory at depth k below `app`; frame 0 is node_modules
    let mut chain = vec![ident(&fx.nm)];
    let mut q = fx.nm.clone();
    for i in 0..(MAX_OPEN_LEVELS * 3) {
        q = q.join(format!("d{i}"));
        chain.push(ident(&q));
    }
    let spy = fx.spy();
    let mut pol = fx.policy();
    pol.max_depth = 100_000;
    let (r, spy) = fx.run_bounded(spy, pol);
    assert!(r.is_ok(), "{r:?}");
    assert!(!exists(&fx.nm));
    let open = spy.open_at_first_unlink.lock().unwrap().clone().unwrap();
    let n = chain.len();
    for (k, id) in chain.iter().enumerate() {
        let should_be_open = k + MAX_OPEN_LEVELS >= n;
        assert_eq!(
            open.contains(id),
            should_be_open,
            "frame {k} of {n}: only the deepest MAX_OPEN_LEVELS frames keep a handle"
        );
    }
    // Re-opening an evicted level keeps the last MAX_OPEN_LEVELS frames, and the level being removed
    // still holds its own handle: MAX_OPEN_LEVELS + 1, plus the root and the parent of the fixture.
    assert_eq!(
        spy.max_open_at_dir_unlink.load(Ordering::SeqCst),
        2 + MAX_OPEN_LEVELS + 1
    );
    let at_descent = spy.max_live_at_first_unlink.load(Ordering::SeqCst);
    // base-level handles of open_verified_root and open_parent are alive too: root, app
    let outside_window = 2;
    assert_eq!(
        at_descent,
        outside_window + MAX_OPEN_LEVELS + 1,
        "the newest frame is pushed before the oldest one outside the window is released"
    );
    assert!(
        spy.max_live.load(Ordering::SeqCst) <= outside_window + MAX_OPEN_LEVELS + 3,
        "climbing back re-opens at most a couple of temporaries"
    );
}

// ------------------------------------------------------------------------------------------------
// rescans: touched bookkeeping, read_dir counts, the skip overflow
// ------------------------------------------------------------------------------------------------

#[test]
fn run_should_rescan_once_after_removing_entries_and_stop_When_the_second_scan_finds_nothing() {
    let fx = Fx::new();
    fx.files(3);
    let spy = fx.spy();
    let f = fx.run(&spy, &fx.policy()).unwrap();
    assert_eq!(f.entries_removed, 4);
    assert_eq!(
        spy.reads.load(Ordering::SeqCst),
        2,
        "one scan that removes, one that finds the directory empty"
    );
}

#[test]
fn note_failure_should_trigger_one_more_scan_When_the_only_entry_fails() {
    let fx = Fx::new();
    fx.files(1);
    let mut spy = fx.spy();
    spy.unlink = Box::new(|_, dir, _| {
        if dir {
            UnlinkAct::Real
        } else {
            UnlinkAct::Err(libc::EACCES)
        }
    });
    let f = err_of(fx.run(&spy, &fx.policy()));
    assert_eq!(f.error, DeleteError::Io(libc::EACCES));
    assert_eq!(
        spy.reads.load(Ordering::SeqCst),
        2,
        "a failure counts as touched: scan, rescan that skips the failed name, finish"
    );
    assert_eq!(spy.unlinks.load(Ordering::SeqCst), 1, "never retried");
}

#[test]
fn scan_should_stop_after_DELETE_BATCH_plus_one_failed_names_When_every_unlink_fails() {
    let fx = Fx::new();
    let n = DELETE_BATCH + 76;
    fx.files(n);
    let mut spy = fx.spy();
    spy.unlink = Box::new(|_, _, _| UnlinkAct::Err(libc::EACCES));
    let f = err_of(fx.run(&spy, &fx.policy()));
    assert_eq!(f.error, DeleteError::Io(libc::EACCES));
    assert_eq!(
        spy.unlinks.load(Ordering::SeqCst),
        DELETE_BATCH + 1,
        "the skip set may hold DELETE_BATCH names; the next failure ends the scan and the level"
    );
    assert_eq!(fs::read_dir(&fx.nm).unwrap().count(), n);
    assert_eq!(
        spy.reads.load(Ordering::SeqCst),
        1,
        "past the overflow the level is finished without another scan"
    );
}

#[test]
fn remove_leaf_should_queue_a_directory_that_replaced_a_listed_file_When_stat_at_says_Dir() {
    let fx = Fx::new();
    write(&fx.nm.join("x/inner"), "y");
    let mut spy = fx.spy();
    spy.as_file = vec![OsString::from("x")];
    let r = fx.run(&spy, &fx.policy());
    assert!(r.is_ok(), "{r:?}");
    assert!(!exists(&fx.nm));
}

#[test]
fn descend_should_rescan_When_a_listed_directory_became_a_file_before_stat_at() {
    let fx = Fx::new();
    write(&fx.nm.join("sub/a"), "x");
    let mut spy = fx.spy();
    spy.stat_at = Box::new(|n, k| {
        if n == OsStr::new("sub") && k == 0 {
            StatAct::Kind(FileKind::File)
        } else {
            StatAct::Real
        }
    });
    let r = fx.run(&spy, &fx.policy());
    assert!(r.is_ok(), "{r:?}");
    assert!(!exists(&fx.nm));
}

#[test]
fn descend_should_rescan_When_stat_at_of_a_listed_directory_reports_ENOENT() {
    let fx = Fx::new();
    write(&fx.nm.join("sub/a"), "x");
    let mut spy = fx.spy();
    spy.stat_at = Box::new(|n, k| {
        if n == OsStr::new("sub") && k == 0 {
            StatAct::Err(libc::ENOENT)
        } else {
            StatAct::Real
        }
    });
    let r = fx.run(&spy, &fx.policy());
    assert!(r.is_ok(), "{r:?}");
    assert!(!exists(&fx.nm));
}

#[test]
fn descend_should_rescan_When_open_dir_of_a_listed_directory_reports_ENOENT() {
    let fx = Fx::new();
    write(&fx.nm.join("sub/a"), "x");
    let mut spy = fx.spy();
    spy.open_dir = Box::new(|n, k| {
        if n == OsStr::new("sub") && k == 0 {
            OpenAct::Err(libc::ENOENT)
        } else {
            OpenAct::Real
        }
    });
    let r = fx.run(&spy, &fx.policy());
    assert!(r.is_ok(), "{r:?}");
    assert!(!exists(&fx.nm));
}

#[test]
fn finish_should_report_IdentityMismatch_When_removing_a_directory_gives_ENOTDIR() {
    let fx = Fx::new();
    fx.files(2);
    let mut spy = fx.spy();
    spy.unlink = Box::new(|n, dir, _| {
        if dir && n == OsStr::new("node_modules") {
            UnlinkAct::Err(libc::ENOTDIR)
        } else {
            UnlinkAct::Real
        }
    });
    let f = err_of(fx.run(&spy, &fx.policy()));
    assert_eq!(f.error, DeleteError::IdentityMismatch);
    assert!(fx.nm.exists());
}

#[test]
fn descend_should_refuse_a_chain_deeper_than_max_depth_and_accept_one_exactly_at_it() {
    // frames[0] is the target; descending from a stack of L frames is refused when L > max_depth.
    let fx = Fx::new();
    fs::create_dir_all(fx.nm.join("a/b/c")).unwrap();
    let spy = fx.spy();
    let mut pol = fx.policy();
    pol.max_depth = 3;
    let r = fx.run(&spy, &pol);
    assert!(
        r.is_ok(),
        "nm/a/b/c is 4 frames and max_depth 3 allows it: {r:?}"
    );
    assert!(!exists(&fx.nm));

    let fx = Fx::new();
    fs::create_dir_all(fx.nm.join("a/b/c/d")).unwrap();
    let spy = fx.spy();
    let mut pol = fx.policy();
    pol.max_depth = 3;
    let f = err_of(fx.run(&spy, &pol));
    assert_eq!(f.error, DeleteError::Io(libc::ENAMETOOLONG));
    assert!(
        exists(&fx.nm.join("a/b/c/d")),
        "the too-deep directory is not removed"
    );
    assert!(exists(&fx.nm.join("a/b/c")));
}

// ------------------------------------------------------------------------------------------------
// dirops.rs: stat bits, read_dir, unlinkat, scope, error mapping
// ------------------------------------------------------------------------------------------------

fn real_dir(ops: &RealDirOps, p: &Path) -> RealDir {
    ops.open_root(p).unwrap()
}

#[test]
fn stat_at_and_fstat_should_report_mode_nlink_blocks_size_kind_and_mnt_id_When_called_on_a_file() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let f = base.join("file");
    fs::write(&f, vec![7u8; 10_000]).unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o640)).unwrap();
    fs::hard_link(&f, base.join("link")).unwrap();
    let md = fs::symlink_metadata(&f).unwrap();
    let ops = RealDirOps::confined_to(&base);
    let d = real_dir(&ops, &base);
    let st = ops.stat_at(&d, OsStr::new("file")).unwrap();
    assert_eq!(st.mode, 0o640, "permission bits only, no file-type bits");
    assert_eq!(st.kind, FileKind::File);
    assert_eq!(st.nlink, 2);
    assert_eq!(st.size, 10_000);
    assert_eq!(st.blocks, md.blocks());
    assert!(st.blocks > 0);
    assert_eq!(st.ident, Identity::of(&md));
    assert_eq!(st.mtime_secs, md.mtime());
    assert_eq!(i64::from(st.mtime_nsec), md.mtime_nsec());
    assert!(
        st.mnt_id.is_some(),
        "statx on this kernel reports the mount id"
    );

    let ds = ops.fstat(&d).unwrap();
    assert_eq!(ds.kind, FileKind::Dir);
    assert_eq!(ds.mode, fs::metadata(&base).unwrap().mode() & 0o7777);
    assert_eq!(ds.ident, Identity::of(&fs::metadata(&base).unwrap()));
    assert_eq!(ds.mnt_id, st.mnt_id, "same filesystem, same mount id");
    assert!(ds.mnt_id.is_some());

    fs::set_permissions(&f, fs::Permissions::from_mode(0o4755)).unwrap();
    let st = ops.stat_at(&d, OsStr::new("file")).unwrap();
    assert_eq!(st.mode, 0o4755, "setuid bit is kept, nothing above 0o7777");
}

#[test]
fn stat_at_should_not_follow_a_symlink_and_report_Symlink_When_name_is_a_link() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    std::os::unix::fs::symlink("/etc", base.join("l")).unwrap();
    let ops = RealDirOps::confined_to(&base);
    let d = real_dir(&ops, &base);
    assert_eq!(
        ops.stat_at(&d, OsStr::new("l")).unwrap().kind,
        FileKind::Symlink
    );
}

#[test]
fn read_dir_should_yield_every_entry_with_kind_and_inode_and_skip_dot_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    fs::write(base.join("f"), "x").unwrap();
    fs::create_dir(base.join("d")).unwrap();
    std::os::unix::fs::symlink("f", base.join("l")).unwrap();
    let ops = RealDirOps::confined_to(&base);
    let d = real_dir(&ops, &base);
    let mut got: Vec<DirEntryInfo> = ops.read_dir(&d).unwrap().map(Result::unwrap).collect();
    got.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<_> = got.iter().map(|e| e.name.to_str().unwrap()).collect();
    assert_eq!(names, ["d", "f", "l"]);
    let kinds: Vec<_> = got.iter().map(|e| e.kind).collect();
    assert_eq!(kinds, [FileKind::Dir, FileKind::File, FileKind::Symlink]);
    for e in &got {
        assert_eq!(
            e.ino,
            fs::symlink_metadata(base.join(&e.name)).unwrap().ino()
        );
    }
}

#[test]
fn unlinkat_should_remove_a_file_or_an_empty_directory_and_refuse_the_wrong_kind() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    fs::write(base.join("f"), "x").unwrap();
    fs::create_dir(base.join("empty")).unwrap();
    fs::create_dir(base.join("full")).unwrap();
    fs::write(base.join("full/x"), "x").unwrap();
    let ops = RealDirOps::confined_to(&base);
    let d = real_dir(&ops, &base);
    ops.unlinkat(&d, OsStr::new("f"), false).unwrap();
    assert!(!exists(&base.join("f")));
    ops.unlinkat(&d, OsStr::new("empty"), true).unwrap();
    assert!(!exists(&base.join("empty")));
    let e = ops.unlinkat(&d, OsStr::new("full"), true).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOTEMPTY));
    assert!(exists(&base.join("full/x")));
    assert!(ops.unlinkat(&d, OsStr::new("full"), false).is_err());
    assert!(exists(&base.join("full")));
}

#[test]
fn confined_to_should_refuse_mutations_outside_its_root_with_a_ScopeViolation_that_displays() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(base.join("inner")).unwrap();
    fs::write(base.join("outside.txt"), "keep").unwrap();
    fs::write(base.join("inner/in.txt"), "x").unwrap();
    let ops = RealDirOps::confined_to(&base.join("inner"));
    let outer = ops.open_root(&base).unwrap();
    let e = ops
        .unlinkat(&outer, OsStr::new("outside.txt"), false)
        .unwrap_err();
    assert!(is_scope_violation(&e));
    assert_eq!(e.to_string(), "mutating call outside the confinement root");
    assert!(exists(&base.join("outside.txt")));
    // a directory opened by name below `outer` lies inside the root and may be mutated
    let inner = ops.open_dir(&outer, OsStr::new("inner")).unwrap();
    ops.unlinkat(&inner, OsStr::new("in.txt"), false).unwrap();
    assert!(!exists(&base.join("inner/in.txt")));
    // re-opening "." keeps the same lexical path
    let again = ops.open_dir(&inner, OsStr::new(".")).unwrap();
    fs::write(base.join("inner/two.txt"), "x").unwrap();
    ops.unlinkat(&again, OsStr::new("two.txt"), false).unwrap();
    let outer_dot = ops.open_dir(&outer, OsStr::new(".")).unwrap();
    assert!(ops
        .unlinkat(&outer_dot, OsStr::new("outside.txt"), false)
        .is_err());
}

#[test]
fn confined_to_should_not_accept_an_unclean_root() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    for bad in [
        base.join("a/../b"),
        base.join("a/b/.."),
        PathBuf::from("relative/dir"),
    ] {
        let r = std::panic::catch_unwind(|| RealDirOps::confined_to(&bad));
        match r {
            Err(_) => {
                let debug = cfg!(debug_assertions);
                assert!(debug, "{bad:?} panics only with debug assertions");
            }
            Ok(ops) => {
                // release builds return an ops value that refuses everything
                let e = ops.open_root(&base).err().expect("refuse_all");
                assert!(is_scope_violation(&e), "{bad:?}");
            }
        }
    }
    // a clean root, and "/" itself, are accepted
    let _ = RealDirOps::confined_to(&base);
    let _ = RealDirOps::confined_to(Path::new("/"));
}

#[test]
fn open_verified_root_should_map_errors_and_return_the_canonical_path_and_a_matching_handle() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    fs::write(base.join("file"), "x").unwrap();
    fs::create_dir(base.join("dir")).unwrap();
    std::os::unix::fs::symlink(base.join("dir"), base.join("link")).unwrap();
    let ops = RealDirOps::confined_to(&base);
    let id = ident(&base.join("dir"));

    let root = open_verified_root(&ops, &base.join("dir"), id).unwrap();
    assert_eq!(root.abs_path(), base.join("dir"));
    assert_eq!(ops.fstat(root.dir()).unwrap().ident, id);

    let e = |p: PathBuf, id: Identity| open_verified_root(&ops, &p, id).err().unwrap();
    assert_eq!(e(base.join("nope"), id), DeleteError::Io(libc::ENOENT));
    assert_eq!(e(base.join("file"), id), DeleteError::Symlink, "ENOTDIR");
    assert_eq!(e(base.join("link"), id), DeleteError::Symlink, "ELOOP");
    assert_eq!(
        e(base.join("dir"), ident(&base)),
        DeleteError::IdentityMismatch
    );
}

// ------------------------------------------------------------------------------------------------
// plain_stat: the fallback when `statx` answers ENOSYS (old kernels, strict seccomp profiles)
// ------------------------------------------------------------------------------------------------

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn block_statx_with_seccomp() {
    let ins = |code: u16, jt: u8, jf: u8, k: u32| libc::sock_filter { code, jt, jf, k };
    let prog = [
        ins(0x20, 0, 0, 4),                      // ld arch
        ins(0x15, 1, 0, 0xc000_003e),            // x86_64 ? goto nr : allow
        ins(0x06, 0, 0, 0x7fff_0000),            // allow
        ins(0x20, 0, 0, 0),                      // ld nr
        ins(0x15, 0, 1, libc::SYS_statx as u32), // statx ? errno : allow
        ins(0x06, 0, 0, 0x0005_0000 | 38),       // errno ENOSYS
        ins(0x06, 0, 0, 0x7fff_0000),            // allow
    ];
    let fprog = libc::sock_fprog {
        len: prog.len() as u16,
        filter: prog.as_ptr().cast_mut(),
    };
    // SAFETY: plain prctl calls with a valid, live BPF program; only this child process is affected.
    unsafe {
        assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
        assert_eq!(
            libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &fprog),
            0
        );
    }
}

/// Runs inside a child process spawned by the next test; a no-op when run directly.
#[test]
fn plain_stat_child_when_statx_is_blocked() {
    if std::env::var_os("DC_MUTANTS_SECCOMP_CHILD").is_none() {
        return;
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let f = base.join("file");
        fs::write(&f, vec![7u8; 10_000]).unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o640)).unwrap();
        fs::hard_link(&f, base.join("link")).unwrap();
        fs::create_dir(base.join("sub")).unwrap();
        let md = fs::symlink_metadata(&f).unwrap();
        let ops = RealDirOps::confined_to(&base);
        let d = ops.open_root(&base).unwrap();
        block_statx_with_seccomp();
        // SAFETY: a raw syscall with null pointers; the filter answers before the kernel looks at them.
        let r = unsafe { libc::syscall(libc::SYS_statx, -1, std::ptr::null::<u8>(), 0, 0, 0) };
        assert_eq!(r, -1);
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ENOSYS)
        );

        let st = ops.stat_at(&d, OsStr::new("file")).unwrap();
        assert_eq!(st.mode, 0o640);
        assert_eq!(st.kind, FileKind::File);
        assert_eq!(st.nlink, 2);
        assert_eq!(st.size, 10_000);
        assert_eq!(st.blocks, md.blocks());
        assert_eq!(st.ident, Identity::of(&md));
        assert_eq!(st.mtime_secs, md.mtime());
        assert_eq!(st.mnt_id, None, "the fallback cannot know the mount id");
        let sub = ops.stat_at(&d, OsStr::new("sub")).unwrap();
        assert_eq!(sub.kind, FileKind::Dir);
        let ds = ops.fstat(&d).unwrap();
        assert_eq!(ds.kind, FileKind::Dir);
        assert_eq!(ds.mode, fs::metadata(&base).unwrap().mode() & 0o7777);
        assert_eq!(ds.ident, Identity::of(&fs::metadata(&base).unwrap()));
    }
}

#[test]
fn stat_info_should_fall_back_to_plain_stat_with_exact_bits_When_statx_returns_ENOSYS() {
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new(exe)
        .args([
            "--exact",
            "plain_stat_child_when_statx_is_blocked",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("DC_MUTANTS_SECCOMP_CHILD", "1")
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("1 passed"),
        "the child test did not run: {text}"
    );
}

#[test]
fn scan_should_buffer_at_most_DELETE_BATCH_subdirectory_names_per_level_When_the_directory_is_wide()
{
    let fx = Fx::new();
    for i in 0..=DELETE_BATCH {
        fs::create_dir(fx.nm.join(format!("d{i:04}"))).unwrap();
    }
    let spy = fx.spy();
    let f = fx.run(&spy, &fx.policy()).unwrap();
    assert!(!exists(&fx.nm));
    assert_eq!(f.entries_removed, DELETE_BATCH as u64 + 2);
    assert_eq!(
        spy.reads.load(Ordering::SeqCst),
        DELETE_BATCH + 1 + 3,
        "one scan per empty child, plus three of the top level: DELETE_BATCH names, the one that did not fit, none"
    );
}

#[test]
fn remove_leaf_should_buffer_at_most_DELETE_BATCH_names_When_that_many_listed_files_are_directories(
) {
    let fx = Fx::new();
    let mut names = Vec::new();
    for i in 0..=DELETE_BATCH {
        let n = format!("d{i:04}");
        fs::create_dir(fx.nm.join(&n)).unwrap();
        names.push(OsString::from(n));
    }
    let mut spy = fx.spy();
    spy.as_file = names;
    let f = fx.run(&spy, &fx.policy()).unwrap();
    assert!(!exists(&fx.nm));
    assert_eq!(f.entries_removed, DELETE_BATCH as u64 + 2);
    assert_eq!(
        spy.reads.load(Ordering::SeqCst),
        DELETE_BATCH + 1 + 3,
        "the 1025th name does not fit the buffer and waits for the next scan"
    );
}

#[test]
fn unlinkat_ENOENT_on_the_only_entry_should_count_as_touched_and_trigger_the_confirming_scan() {
    let fx = Fx::new();
    fx.files(1);
    let mut spy = fx.spy();
    spy.unlink = Box::new(|_, dir, _| {
        if dir {
            UnlinkAct::Real
        } else {
            UnlinkAct::ThenErr(libc::ENOENT)
        }
    });
    let f = fx.run(&spy, &fx.policy()).unwrap();
    assert!(!exists(&fx.nm));
    assert_eq!(f.entries_removed, 1, "only the directory is counted");
    assert_eq!(
        spy.reads.load(Ordering::SeqCst),
        2,
        "a vanished entry is a change: scan again before removing the directory"
    );
}

fn dir_unlink_fails_with(errno_value: i32) -> (DeleteFailure, bool) {
    let fx = Fx::new();
    fx.files(2);
    let mut spy = fx.spy();
    spy.unlink = Box::new(move |n, dir, _| {
        if dir && n == OsStr::new("node_modules") {
            UnlinkAct::Err(errno_value)
        } else {
            UnlinkAct::Real
        }
    });
    let f = err_of(fx.run(&spy, &fx.policy()));
    (f, spy.abort.is_set())
}

#[test]
fn finish_should_abort_on_ENOSPC_EDQUOT_EIO_and_only_on_those_When_removing_the_directory_fails() {
    for e in [libc::ENOSPC, libc::EDQUOT, libc::EIO] {
        let (f, aborted) = dir_unlink_fails_with(e);
        assert_eq!(f.error, DeleteError::NoSpace, "errno {e}");
        assert!(aborted, "errno {e} sets the abort flag");
        assert_eq!(f.freed.entries_removed, 2, "the two files were counted");
    }
    let (f, aborted) = dir_unlink_fails_with(libc::EACCES);
    assert_eq!(f.error, DeleteError::Io(libc::EACCES));
    assert!(!aborted, "a per-directory error is not an abort");
}

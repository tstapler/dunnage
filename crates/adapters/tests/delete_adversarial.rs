#![allow(
    non_snake_case,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::needless_pass_by_value,
    clippy::type_complexity,
    clippy::similar_names,
    clippy::items_after_statements
)]
//! Adversarial suite for `delete_tree` (ADR-003, Story 2.1.1, Task 2.1.1b).
//!
//! Written by an independent blind author from the frozen contract (module doc of
//! `adapters::delete`), `adapters::dirops` / `adapters::testing` signatures, the `core` data types,
//! invariants I1-I8 and spike 0.2.5 only. No `delete_tree` body was read. Every scenario lives in a
//! tempdir under the std temp dir; `RealDirOps::confined_to(base)` refuses a mutation outside that
//! tempdir, `Fx::new` asserts the confinement root before anything is created, and tests that run
//! `delete_tree` hold a process-wide lock so the `/proc/self/fd` count of A-26 is not polluted.
//!
//! Invariants: I1 never follow a symlink; I2 never cross `st_dev` (or `mnt_id`); I3 never act on a path
//! string after the fd is open; I4 abort on ENOSPC, return the partial `Freed`; I5 hardlinks counted once
//! per `(dev, ino)`; I6 refuse floor paths (rule F); I7 idempotent on ENOENT; I8 no stray unlink elsewhere.
//!
//! Case table (checked against the `#[test]` names by `scripts/check-case-table.sh`):
//!
//! CASE-TABLE-BEGIN
//! A-01 | I1 | symlink_at_every_depth_pointing_outside_root_should_be_unlinked_not_followed_When_deleting_tree
//! A-02 | I1,I3 | symlink_swapped_for_directory_between_open_dir_and_read_dir_should_not_be_followed_When_hook_AfterOpenDir_races
//! A-03 | I1,I3 | entry_replaced_by_symlink_before_unlinkat_should_unlink_the_link_only_When_hook_BeforeUnlinkat
//! A-04 | I1 | symlinked_root_target_should_return_Symlink_or_IdentityMismatch_and_delete_nothing_When_root_is_a_link
//! A-05 | I6 | names_empty_dot_dotdot_slash_should_be_ProtectedPath_or_OutsideScope_with_zero_opens_When_name_given
//! A-06 | I3 | open_parent_should_return_OutsideScope_with_zero_opendir_When_rel_parent_is_dotdot_dotdot_ssh_a_dotdot_b_etc_aa_a_dot_b_dot_a_or_a_slash
//! A-07 | I3 | policy_target_with_dotdot_to_sibling_should_return_OutsideScope_and_delete_nothing_When_valid_parent_for_sibling
//! A-08 | I1 | symlinked_intermediate_component_should_make_open_parent_return_Symlink_When_npm_cache_or_cargo_registry_linked
//! A-09 | I4 | enospc_on_5th_unlinkat_across_four_concurrent_trees_should_set_abort_and_stop_all_unlinks_When_shared_AbortFlag
//! A-10 | I4,I5 | bytes_freed_before_enospc_should_be_in_DeleteFailure_freed_When_failure_mid_tree
//! A-11 | I2 | subdir_reported_on_other_dev_or_other_mnt_id_should_be_skipped_CrossDevice_and_siblings_removed_When_report_dev_42_or_report_mnt_id_9
//! A-12 | I3 | ten_thousand_level_nesting_should_complete_without_stack_overflow_When_deleting
//! A-13 | I1 | fifo_socket_chardev_nodes_should_be_unlinked_without_opening_them_When_in_tree
//! A-14 | I5 | hardlink_pair_should_count_blocks_once_and_inodes_freed_2_entries_3_When_two_links_one_inode
//! A-15 | I3 | read_only_directories_should_be_chmodded_on_verified_fd_and_removed_When_mode_0555
//! A-16 | I3 | names_with_newlines_and_invalid_utf8_should_be_removed_exactly_When_in_tree
//! A-17 | I4,I7 | enotempty_from_concurrent_writer_should_map_to_Busy_without_retry_loop_When_before_unlinkat_creates_file
//! A-18 | I7 | enoent_during_delete_should_be_treated_as_removed_and_idempotent_on_second_call_When_entry_vanishes
//! A-19 | I3,I6 | verified_parent_for_dir_X_with_target_under_dir_Y_should_return_OutsideScope_even_when_Y_has_same_name_When_paired
//! A-20 | I6 | missing_sibling_marker_should_return_OutsideScope_When_require_sibling_any_not_found
//! A-21 | I6,I8 | fixture_hash_before_and_after_should_match_When_delete_tree_refused_by_validate
//! A-23 | I3 | identity_mismatch_between_expect_and_fstat_should_return_IdentityMismatch_and_delete_nothing_When_dir_replaced
//! A-24 | I1,I5 | hardlink_to_file_outside_tree_should_unlink_only_the_in_tree_name_When_nlink_gt_1
//! A-25 | I3 | directory_mode_000_unreadable_should_return_Io_EACCES_with_partial_freed_and_continue_siblings_When_in_tree
//! A-26 | I8 | fd_count_should_return_to_baseline_When_delete_tree_returns_ok_or_err
//! CASE-TABLE-END

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use dunnage_adapters::testing::{At, Event, FaultDirOps, HookCtx, Op};
use dunnage_adapters::{
    delete_tree, open_parent, open_verified_root, DirOps, RealDirOps, VerifiedParent, VerifiedRoot,
};
use dunnage_core::{
    AbortFlag, AllocatedBytes, AllowedTarget, ApplyToken, DeleteError, DeleteFailure, DeletePolicy,
    Freed, Identity, NamePattern, RootDev,
};

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

static SERIAL: Mutex<()> = Mutex::new(());

type Fault = FaultDirOps<RealDirOps>;

/// Tempdir fixture. Layout under `base`:
/// `home/{code/app/{package.json,node_modules},Documents/doc.txt,.ssh/id}` and `outside/keep.txt`.
struct Fx {
    tmp: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    code: PathBuf,
    app: PathBuf,
    nm: PathBuf,
    outside: PathBuf,
    _lock: MutexGuard<'static, ()>,
}

/// The only places a test may create or delete anything: the std temp dir or `$HOME/.cache/dc-fixtures`.
fn assert_confined(p: &Path) {
    let tmp = fs::canonicalize(std::env::temp_dir()).unwrap();
    let dc = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/dc-fixtures"));
    let in_tmp = p.starts_with(&tmp) && p != tmp;
    let in_dc = dc.is_some_and(|d| p.starts_with(&d) && p != d);
    assert!(
        in_tmp || in_dc,
        "refusing to operate outside the confinement roots: {}",
        p.display()
    );
    assert!(p.is_absolute());
}

fn write_file(p: &Path, len: usize) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    let mut f = fs::File::create(p).unwrap();
    f.write_all(&vec![0xA5u8; len]).unwrap();
    f.sync_all().unwrap();
}

fn write_text(p: &Path, s: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, s).unwrap();
}

impl Fx {
    fn new() -> Fx {
        let lock = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let tmp = tempfile::Builder::new()
            .prefix("dc-adv-")
            .tempdir()
            .unwrap();
        let base = tmp.path().canonicalize().unwrap();
        assert_confined(&base);
        let home = base.join("home");
        let code = home.join("code");
        let app = code.join("app");
        let nm = app.join("node_modules");
        let outside = base.join("outside");
        fs::create_dir_all(&nm).unwrap();
        write_text(&app.join("package.json"), "{}");
        write_text(&outside.join("keep.txt"), "keep me");
        write_text(&home.join("Documents/doc.txt"), "precious");
        write_text(&home.join(".ssh/id"), "secret");
        Fx {
            tmp,
            base,
            home,
            code,
            app,
            nm,
            outside,
            _lock: lock,
        }
    }

    fn ops(&self) -> RealDirOps {
        RealDirOps::confined_to(&self.base)
    }

    fn fault(&self) -> Fault {
        FaultDirOps::wrap(self.ops()).build()
    }

    fn policy(&self) -> DeletePolicy {
        self.policy_for(&self.nm)
    }

    fn policy_for(&self, target: &Path) -> DeletePolicy {
        DeletePolicy {
            root_dev: RootDev(dev_of(&self.base)),
            root_mnt: None,
            home: self.home.clone(),
            home_id: ident(&self.home),
            scan_roots: vec![self.code.clone()],
            allowed: AllowedTarget::Within {
                root: self.code.clone(),
                name: NamePattern::NodeModules,
            },
            target: target.to_path_buf(),
            require_sibling_any: vec![OsString::from("package.json")],
            max_depth: 256,
            restore_owner_write: false,
        }
    }

    /// `node_modules` with `n` synced files of 8 KiB each, returning the file paths.
    fn fill_flat(&self, n: usize) -> Vec<PathBuf> {
        (0..n)
            .map(|i| {
                let p = self.nm.join(format!("f{i:03}"));
                write_file(&p, 8192);
                p
            })
            .collect()
    }

    /// Run the standard `home/code/app` + `node_modules` delete.
    fn run<D: DirOps>(
        &self,
        ops: &D,
        pol: &DeletePolicy,
        abort: &AbortFlag,
    ) -> Result<Freed, DeleteFailure> {
        run_at(
            ops,
            &self.base,
            "home/code/app",
            "node_modules",
            ident(&self.nm),
            pol,
            abort,
        )
    }

    /// Everything under `base` except the skipped subtrees.
    fn snap_except(&self, skip: &[&Path]) -> BTreeMap<PathBuf, String> {
        let mut out = BTreeMap::new();
        snap_into(&self.base, &self.base, skip, &mut out);
        out
    }

    fn snap(&self) -> BTreeMap<PathBuf, String> {
        self.snap_except(&[])
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        assert_confined(&self.base);
        make_writable(&self.base);
        let _ = &self.tmp;
    }
}

fn run_at<D: DirOps>(
    ops: &D,
    base: &Path,
    rel_parent: &str,
    name: &str,
    expect: Identity,
    pol: &DeletePolicy,
    abort: &AbortFlag,
) -> Result<Freed, DeleteFailure> {
    let root = open_verified_root(ops, base, ident(base)).unwrap();
    let parent = open_parent(ops, &root, Path::new(rel_parent)).unwrap();
    delete_tree(
        ops,
        &parent,
        OsStr::new(name),
        expect,
        pol,
        abort,
        &ApplyToken::for_tests(),
    )
}

fn ident(p: &Path) -> Identity {
    Identity::of(&fs::symlink_metadata(p).unwrap())
}

fn dev_of(p: &Path) -> u64 {
    fs::symlink_metadata(p).unwrap().dev()
}

fn bytes_of(p: &Path) -> u64 {
    fs::symlink_metadata(p).unwrap().blocks() * 512
}

fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

fn is_link(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

fn content_hash(p: &Path) -> u64 {
    let mut h = DefaultHasher::new();
    fs::read(p).unwrap_or_default().hash(&mut h);
    h.finish()
}

/// Describes every entry below `cur` (never following symlinks) as a string; keys are relative to `root`.
fn snap_into(root: &Path, cur: &Path, skip: &[&Path], out: &mut BTreeMap<PathBuf, String>) {
    let Ok(rd) = fs::read_dir(cur) else {
        return;
    };
    let mut names: Vec<_> = rd.map(|e| e.unwrap().path()).collect();
    names.sort();
    for p in names {
        if skip.iter().any(|s| *s == p) {
            continue;
        }
        let m = fs::symlink_metadata(&p).unwrap();
        let ft = m.file_type();
        let rel = p.strip_prefix(root).unwrap().to_path_buf();
        let desc = if ft.is_symlink() {
            format!("l {}", fs::read_link(&p).unwrap().display())
        } else if ft.is_dir() {
            format!("d {:o}", m.mode() & 0o7777)
        } else if ft.is_file() {
            format!(
                "f {:o} {} {:x}",
                m.mode() & 0o7777,
                m.len(),
                content_hash(&p)
            )
        } else {
            format!("o {:o}", m.mode() & 0o170_000)
        };
        out.insert(rel, desc);
        if ft.is_dir() {
            snap_into(root, &p, skip, out);
        }
    }
}

/// Test cleanup only: give the owner rwx back on every directory under `base`, never following links.
fn make_writable(base: &Path) {
    assert_confined(base);
    fn walk(p: &Path) {
        let Ok(m) = fs::symlink_metadata(p) else {
            return;
        };
        if m.file_type().is_dir() {
            let _ = fs::set_permissions(p, fs::Permissions::from_mode((m.mode() & 0o7777) | 0o700));
            if let Ok(rd) = fs::read_dir(p) {
                for e in rd.flatten() {
                    walk(&e.path());
                }
            }
        }
    }
    walk(base);
}

fn opened(ops: &Fault) -> usize {
    ops.calls(Op::OpenDir)
}

/// Run `f` on a thread and fail instead of hanging forever.
fn with_timeout<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(secs))
        .expect("delete_tree did not return in time (opened a FIFO or looped?)")
}

/// A chain of `depth` nested directories named `d`, built with `openat` so no path exceeds PATH_MAX.
fn build_chain(dir: &Path, depth: usize) {
    use rustix::fs::{mkdirat, open, openat, Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut cur = open(dir, flags, Mode::empty()).unwrap();
    for _ in 0..depth {
        mkdirat(&cur, "d", Mode::from_bits_truncate(0o755)).unwrap();
        let next = openat(&cur, "d", flags, Mode::empty()).unwrap();
        cur = next;
    }
    let leaf = openat(
        &cur,
        "leaf",
        OFlags::CREATE | OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o644),
    )
    .unwrap();
    drop(leaf);
}

/// Test cleanup for `build_chain` when the engine under test failed: removes the chain in chunks of at
/// most 300 levels, moving the rest of the chain up to `top` first so no more than 300 fds are open.
fn unwind_chain(top: &Path) {
    use rustix::fs::{open, openat, renameat, unlinkat, AtFlags, Mode, OFlags};
    assert_confined(top);
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    if !exists(top) {
        return;
    }
    let topfd = open(top, flags, Mode::empty()).unwrap();
    loop {
        let mut fds = Vec::new();
        loop {
            let next = match fds.last() {
                None => openat(&topfd, "d", flags, Mode::empty()),
                Some(last) => openat(last, "d", flags, Mode::empty()),
            };
            match next {
                Ok(fd) if fds.len() < 300 => fds.push(fd),
                Ok(fd) => {
                    fds.push(fd);
                    break;
                }
                Err(_) => break,
            }
        }
        if fds.is_empty() {
            return;
        }
        let more = openat(fds.last().unwrap(), "d", flags, Mode::empty()).is_ok();
        if more {
            renameat(fds.last().unwrap(), "d", &topfd, "next").unwrap();
        }
        for i in (0..fds.len()).rev() {
            let _ = unlinkat(&fds[i], "leaf", AtFlags::empty());
            let parent = if i == 0 { &topfd } else { &fds[i - 1] };
            let _ = unlinkat(parent, "d", AtFlags::REMOVEDIR);
        }
        if !more {
            return;
        }
        renameat(&topfd, "next", &topfd, "d").unwrap();
    }
}

fn err_kind(r: &Result<Freed, DeleteFailure>) -> Option<DeleteError> {
    r.as_ref().err().map(|f| f.error.clone())
}

fn is_refusal(e: &DeleteError) -> bool {
    matches!(e, DeleteError::ProtectedPath | DeleteError::OutsideScope)
}

fn fd_count() -> usize {
    // macOS has no /proc; /dev/fd lists the open descriptors there too
    let dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    fs::read_dir(dir).unwrap().count()
}

// ---------------------------------------------------------------------------------------------
// A-01 .. A-26
// ---------------------------------------------------------------------------------------------

#[test]
fn symlink_at_every_depth_pointing_outside_root_should_be_unlinked_not_followed_When_deleting_tree()
{
    let fx = Fx::new();
    let nm = &fx.nm;
    let abs_outside = fx.outside.clone();
    // depth 0..4, each with a different flavour of escaping link
    fs::create_dir_all(nm.join("a/b/c/d")).unwrap();
    write_text(&nm.join("file0"), "x");
    symlink("../../../../outside", nm.join("link_rel_dir0")).unwrap();
    symlink(&abs_outside, nm.join("a/link_abs_dir1")).unwrap();
    symlink(abs_outside.join("keep.txt"), nm.join("a/b/link_file2")).unwrap();
    symlink(
        abs_outside.join("does-not-exist"),
        nm.join("a/b/link_dangling2"),
    )
    .unwrap();
    symlink(&fx.app, nm.join("a/b/c/link_to_ancestor3")).unwrap();
    symlink(&fx.home, nm.join("a/b/c/d/link_to_home4")).unwrap();
    symlink("/", nm.join("a/b/c/d/link_to_slash4")).unwrap();
    let before = fx.snap_except(&[&fx.nm]);

    let ops = fx.fault();
    let r = fx.run(&ops, &fx.policy(), &AbortFlag::new());

    assert!(r.is_ok(), "{r:?}");
    assert!(!exists(&fx.nm), "node_modules must be gone");
    assert_eq!(
        fx.snap_except(&[&fx.nm]),
        before,
        "something outside the tree changed"
    );
    assert_eq!(
        fs::read_to_string(fx.outside.join("keep.txt")).unwrap(),
        "keep me"
    );
    assert!(exists(&fx.app.join("package.json")));
}

fn race_swap_case(victim_rel: &str) {
    let fx = Fx::new();
    let nm = &fx.nm;
    fs::create_dir_all(nm.join("sub/deep")).unwrap();
    write_text(&nm.join("sub/f1"), "1");
    write_text(&nm.join("sub/deep/f2"), "2");
    write_text(&nm.join("top.txt"), "t");
    let stash = fx.base.join("stash");
    let outside = fx.outside.clone();
    let swapped = Arc::new(AtomicBool::new(false));
    let victim = PathBuf::from("home/code/app/node_modules").join(victim_rel);
    let flag = swapped.clone();
    let ops = FaultDirOps::wrap(fx.ops())
        .hook(Event::AfterOpenDir, move |ctx: &HookCtx| {
            if ctx.rel_path == victim && !flag.swap(true, Ordering::SeqCst) {
                let at = ctx.base_abs.join(&ctx.rel_path);
                fs::rename(&at, &stash).unwrap();
                symlink(&outside, &at).unwrap();
            }
        })
        .build();
    let before = fx.snap_except(&[&fx.nm, &fx.base.join("stash")]);

    let r = fx.run(&ops, &fx.policy(), &AbortFlag::new());

    assert!(
        swapped.load(Ordering::SeqCst),
        "the race hook never fired for {victim_rel}"
    );
    assert_eq!(
        fx.snap_except(&[&fx.nm, &fx.base.join("stash")]),
        before,
        "content outside the tree changed after the swap at {victim_rel}: {r:?}"
    );
    assert_eq!(
        fs::read_to_string(fx.outside.join("keep.txt")).unwrap(),
        "keep me"
    );
    if r.is_ok() {
        assert!(!exists(&fx.nm), "Ok means node_modules is gone");
    }
}

#[test]
fn symlink_swapped_for_directory_between_open_dir_and_read_dir_should_not_be_followed_When_hook_AfterOpenDir_races(
) {
    race_swap_case("sub");
    race_swap_case("sub/deep");
}

#[test]
fn entry_replaced_by_symlink_before_unlinkat_should_unlink_the_link_only_When_hook_BeforeUnlinkat()
{
    // file replaced by a link to an outside file
    {
        let fx = Fx::new();
        write_text(&fx.nm.join("victim"), "v");
        write_text(&fx.nm.join("other"), "o");
        let outside_file = fx.outside.join("keep.txt");
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let target = PathBuf::from("home/code/app/node_modules/victim");
        let ops = FaultDirOps::wrap(fx.ops())
            .hook(Event::BeforeUnlinkat, move |ctx: &HookCtx| {
                if ctx.rel_path == target && !flag.swap(true, Ordering::SeqCst) {
                    let at = ctx.base_abs.join(&ctx.rel_path);
                    fs::remove_file(&at).unwrap();
                    symlink(&outside_file, &at).unwrap();
                }
            })
            .build();
        let before = fx.snap_except(&[&fx.nm]);
        let r = fx.run(&ops, &fx.policy(), &AbortFlag::new());
        assert!(done.load(Ordering::SeqCst));
        assert!(r.is_ok(), "{r:?}");
        assert!(!exists(&fx.nm));
        assert_eq!(fx.snap_except(&[&fx.nm]), before);
        assert_eq!(
            fs::read_to_string(fx.outside.join("keep.txt")).unwrap(),
            "keep me"
        );
    }
    // empty directory replaced by a link to a populated outside directory just before its rmdir
    {
        let fx = Fx::new();
        fs::create_dir_all(fx.nm.join("vdir")).unwrap();
        write_text(&fx.nm.join("other"), "o");
        let outside_dir = fx.outside.clone();
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let target = PathBuf::from("home/code/app/node_modules/vdir");
        let ops = FaultDirOps::wrap(fx.ops())
            .hook(Event::BeforeUnlinkat, move |ctx: &HookCtx| {
                if ctx.rel_path == target && !flag.swap(true, Ordering::SeqCst) {
                    let at = ctx.base_abs.join(&ctx.rel_path);
                    fs::remove_dir(&at).unwrap();
                    symlink(&outside_dir, &at).unwrap();
                }
            })
            .build();
        let before = fx.snap_except(&[&fx.nm]);
        let r = fx.run(&ops, &fx.policy(), &AbortFlag::new());
        assert!(done.load(Ordering::SeqCst));
        assert_eq!(
            fx.snap_except(&[&fx.nm]),
            before,
            "outside content touched: {r:?}"
        );
        assert_eq!(
            fs::read_to_string(fx.outside.join("keep.txt")).unwrap(),
            "keep me"
        );
        if r.is_ok() {
            assert!(!exists(&fx.nm));
        }
    }
    // the control: with no swap the same layout deletes cleanly
    let fx = Fx::new();
    write_text(&fx.nm.join("victim"), "v");
    let r = fx.run(&fx.fault(), &fx.policy(), &AbortFlag::new());
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn symlinked_root_target_should_return_Symlink_or_IdentityMismatch_and_delete_nothing_When_root_is_a_link(
) {
    let fx = Fx::new();
    fs::remove_dir(&fx.nm).unwrap();
    let real = fx.outside.join("realnm");
    write_text(&real.join("x"), "x");
    write_text(&real.join("sub/y"), "y");
    symlink(&real, &fx.nm).unwrap();
    let before = fx.snap();
    for expect in [ident(&fx.nm), Identity::of(&fs::metadata(&fx.nm).unwrap())] {
        let ops = fx.fault();
        let r = run_at(
            &ops,
            &fx.base,
            "home/code/app",
            "node_modules",
            expect,
            &fx.policy(),
            &AbortFlag::new(),
        );
        assert!(
            matches!(
                err_kind(&r),
                Some(DeleteError::Symlink | DeleteError::IdentityMismatch)
            ),
            "{r:?}"
        );
        assert_eq!(ops.calls(Op::Unlinkat), 0);
        assert_eq!(
            fx.snap(),
            before,
            "a symlinked target must leave everything in place"
        );
        assert!(is_link(&fx.nm));
    }
    // a symlinked verified root is refused as well
    let link = fx.outside.join("baselink");
    symlink(&fx.base, &link).unwrap();
    let e = open_verified_root(&fx.ops(), &link, ident(&fx.base)).err();
    assert!(
        matches!(
            e,
            Some(DeleteError::Symlink | DeleteError::IdentityMismatch)
        ),
        "{e:?}"
    );
    // control: a real directory at the same name deletes
    fs::remove_file(&fx.nm).unwrap();
    fs::create_dir(&fx.nm).unwrap();
    let r = fx.run(&fx.fault(), &fx.policy(), &AbortFlag::new());
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn names_empty_dot_dotdot_slash_should_be_ProtectedPath_or_OutsideScope_with_zero_opens_When_name_given(
) {
    let fx = Fx::new();
    write_text(&fx.nm.join("f"), "f");
    let before = fx.snap();
    let ops = fx.fault();
    let root = open_verified_root(&ops, &fx.base, ident(&fx.base)).unwrap();
    let parent = open_parent(&ops, &root, Path::new("home/code/app")).unwrap();
    let baseline = opened(&ops);
    for name in [
        "",
        ".",
        "..",
        "a/b",
        "/",
        "../..",
        "node_modules/",
        "./node_modules",
        "node_modules/..",
        "../app/node_modules",
    ] {
        let target = parent.abs_path().join(name);
        let mut pol = fx.policy_for(&target);
        pol.allowed = AllowedTarget::Exact(target.clone());
        let expect = fs::metadata(&target).map_or_else(|_| ident(&fx.app), |m| Identity::of(&m));
        let r = delete_tree(
            &ops,
            &parent,
            OsStr::new(name),
            expect,
            &pol,
            &AbortFlag::new(),
            &ApplyToken::for_tests(),
        );
        let e = err_kind(&r);
        assert!(e.as_ref().is_some_and(is_refusal), "name {name:?}: {r:?}");
        assert_eq!(opened(&ops), baseline, "name {name:?} opened a directory");
        assert_eq!(ops.calls(Op::Unlinkat), 0);
        assert_eq!(fx.snap(), before, "name {name:?} changed the tree");
    }
    // control
    let r = delete_tree(
        &ops,
        &parent,
        OsStr::new("node_modules"),
        ident(&fx.nm),
        &fx.policy(),
        &AbortFlag::new(),
        &ApplyToken::for_tests(),
    );
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn open_parent_should_return_OutsideScope_with_zero_opendir_When_rel_parent_is_dotdot_dotdot_ssh_a_dotdot_b_etc_aa_a_dot_b_dot_a_or_a_slash(
) {
    let fx = Fx::new();
    // root is `home`; its sibling `outside` holds a node_modules that must never be opened or touched
    let sibling_nm = fx.outside.join("node_modules");
    write_text(&sibling_nm.join("pkg/index.js"), "x");
    write_text(&fx.outside.join("package.json"), "{}");
    fs::create_dir_all(fx.home.join("a/aa")).unwrap();
    fs::create_dir_all(fx.home.join("b")).unwrap();
    let before = fx.snap();
    let ops = fx.fault();
    let root = open_verified_root(&ops, &fx.home, ident(&fx.home)).unwrap();
    for p in [
        "../.ssh",
        "../outside",
        "../outside/node_modules",
        "a/../b",
        "/etc",
        "a//b",
        "a/./b",
        "./a",
        "a/",
        "..",
        "a/..",
        "/",
    ] {
        let e = open_parent(&ops, &root, Path::new(p)).err();
        assert_eq!(e, Some(DeleteError::OutsideScope), "rel_parent {p:?}");
        assert_eq!(opened(&ops), 0, "rel_parent {p:?} opened a directory");
    }
    assert_eq!(fx.snap(), before);
    // control: the engine still works from the same root after the refusals
    let parent = open_parent(&ops, &root, Path::new("code/app")).unwrap();
    let r = delete_tree(
        &ops,
        &parent,
        OsStr::new("node_modules"),
        ident(&fx.nm),
        &fx.policy(),
        &AbortFlag::new(),
        &ApplyToken::for_tests(),
    );
    assert!(r.is_ok(), "{r:?}");
    assert!(exists(&sibling_nm.join("pkg/index.js")));
}

#[test]
fn policy_target_with_dotdot_to_sibling_should_return_OutsideScope_and_delete_nothing_When_valid_parent_for_sibling(
) {
    let fx = Fx::new();
    let sibling = fx.base.join("sibling");
    write_text(&sibling.join("node_modules/pkg/index.js"), "x");
    write_text(&sibling.join("package.json"), "{}");
    write_text(&fx.nm.join("mine"), "m");
    let before = fx.snap();
    let ops = fx.fault();
    let sibling_nm = sibling.join("node_modules");

    // (a) valid parent for the sibling, target spelled through the verified root with `..`
    let sroot = open_verified_root(&ops, &sibling, ident(&sibling)).unwrap();
    let sparent = open_parent(&ops, &sroot, Path::new("")).unwrap();
    let dotted = fx.home.join("../sibling/node_modules");
    let r = delete_tree(
        &ops,
        &sparent,
        OsStr::new("node_modules"),
        ident(&sibling_nm),
        &fx.policy_for(&dotted),
        &AbortFlag::new(),
        &ApplyToken::for_tests(),
    );
    assert_eq!(err_kind(&r), Some(DeleteError::OutsideScope), "{r:?}");
    assert_eq!(fx.snap(), before);

    // (b) the same attack with a non-canonical verified root, so that parent.abs.join(name) == target as strings
    let unclean_base = fx.home.join("../sibling");
    match open_verified_root(&ops, &unclean_base, ident(&sibling)) {
        Err(_) => {}
        Ok(uroot) => {
            let uparent = open_parent(&ops, &uroot, Path::new("")).unwrap();
            let target = uparent.abs_path().join("node_modules");
            let r = delete_tree(
                &ops,
                &uparent,
                OsStr::new("node_modules"),
                ident(&sibling_nm),
                &fx.policy_for(&target),
                &AbortFlag::new(),
                &ApplyToken::for_tests(),
            );
            assert_eq!(err_kind(&r), Some(DeleteError::OutsideScope), "{r:?}");
        }
    }
    assert_eq!(ops.calls(Op::Unlinkat), 0);
    assert_eq!(fx.snap(), before);

    // control
    let r = fx.run(&ops, &fx.policy(), &AbortFlag::new());
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn symlinked_intermediate_component_should_make_open_parent_return_Symlink_When_npm_cache_or_cargo_registry_linked(
) {
    let fx = Fx::new();
    let h = &fx.home;
    // ~/.npm -> ~/Documents/old-npm ; ~/.cache -> outside/ssd-cache ; ~/.cargo/registry -> outside/elsewhere
    write_text(&h.join("Documents/old-npm/_cacache/content/blob"), "npm");
    symlink(h.join("Documents/old-npm"), h.join(".npm")).unwrap();
    write_text(
        &fx.outside.join("ssd-cache/ms-playwright/browser.bin"),
        "pw",
    );
    symlink(fx.outside.join("ssd-cache"), h.join(".cache")).unwrap();
    write_text(&fx.outside.join("elsewhere/src/crate/lib.rs"), "rs");
    fs::create_dir_all(h.join(".cargo")).unwrap();
    symlink(fx.outside.join("elsewhere"), h.join(".cargo/registry")).unwrap();
    let before = fx.snap();
    let ops = fx.fault();
    let root = open_verified_root(&ops, &fx.base, ident(&fx.base)).unwrap();
    for rel in [
        "home/.npm",
        "home/.cache",
        "home/.cargo/registry",
        "home/.npm/_cacache",
        "home/.cache/ms-playwright",
        "home/.cargo/registry/src",
    ] {
        let e = open_parent(&ops, &root, Path::new(rel)).err();
        assert_eq!(e, Some(DeleteError::Symlink), "rel_parent {rel}");
    }
    assert_eq!(fx.snap(), before);
    // control
    let r = fx.run(&ops, &fx.policy(), &AbortFlag::new());
    assert!(r.is_ok(), "{r:?}");
    assert!(exists(&h.join("Documents/old-npm/_cacache/content/blob")));
    assert!(exists(
        &fx.outside.join("ssd-cache/ms-playwright/browser.bin")
    ));
    assert!(exists(&fx.outside.join("elsewhere/src/crate/lib.rs")));
}

#[test]
fn enospc_on_5th_unlinkat_across_four_concurrent_trees_should_set_abort_and_stop_all_unlinks_When_shared_AbortFlag(
) {
    const TREES: usize = 4;
    const FILES: usize = 200;
    let fx = Fx::new();
    let mut nms = Vec::new();
    for i in 0..TREES {
        let app = fx.code.join(format!("app{i}"));
        write_text(&app.join("package.json"), "{}");
        let nm = app.join("node_modules");
        for j in 0..FILES {
            write_text(&nm.join(format!("f{j:03}")), "x");
        }
        nms.push(nm);
    }
    let abort = Arc::new(AbortFlag::new());
    let late = Arc::new(AtomicUsize::new(0));
    let (a2, l2) = (abort.clone(), late.clone());
    let ops = FaultDirOps::wrap(fx.ops())
        .enospc_on_nth_unlinkat(5)
        .hook(Event::BeforeUnlinkat, move |_: &HookCtx| {
            if a2.is_set() {
                l2.fetch_add(1, Ordering::SeqCst);
            }
        })
        .build();
    let results: Vec<Result<Freed, DeleteFailure>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..TREES)
            .map(|i| {
                let ops = &ops;
                let abort = &abort;
                let base = &fx.base;
                let nm = &nms[i];
                let pol = fx.policy_for(nm);
                s.spawn(move || {
                    run_at(
                        ops,
                        base,
                        &format!("home/code/app{i}"),
                        "node_modules",
                        ident(nm),
                        &pol,
                        abort,
                    )
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(abort.is_set(), "ENOSPC must set the shared AbortFlag");
    assert!(
        results
            .iter()
            .any(|r| err_kind(r) == Some(DeleteError::NoSpace)),
        "{results:?}"
    );
    assert!(
        late.load(Ordering::SeqCst) < TREES,
        "{} unlinkat calls were issued after the abort was visible (at most one in flight per other tree)",
        late.load(Ordering::SeqCst)
    );
    assert!(
        ops.calls(Op::Unlinkat) < TREES * FILES,
        "the engine kept unlinking after ENOSPC"
    );
    assert!(
        nms.iter().any(|nm| exists(nm)),
        "an aborted run cannot have removed every tree"
    );
}

#[test]
fn bytes_freed_before_enospc_should_be_in_DeleteFailure_freed_When_failure_mid_tree() {
    let fx = Fx::new();
    let files = fx.fill_flat(20);
    let size: BTreeMap<PathBuf, u64> = files.iter().map(|p| (p.clone(), bytes_of(p))).collect();
    assert!(
        size.values().all(|b| *b > 0),
        "fixture filesystem reports no blocks for 8 KiB files"
    );
    let ops = FaultDirOps::wrap(fx.ops())
        .enospc_on_nth_unlinkat(10)
        .build();
    let abort = AbortFlag::new();

    let r = fx.run(&ops, &fx.policy(), &abort);

    let f = r.unwrap_err();
    assert_eq!(f.error, DeleteError::NoSpace);
    assert!(abort.is_set());
    let removed: Vec<_> = files.iter().filter(|p| !exists(p)).collect();
    assert_eq!(
        removed.len(),
        9,
        "the 10th unlinkat failed, so exactly 9 files were removed"
    );
    let expect: u64 = removed.iter().map(|p| size[*p]).sum();
    assert_eq!(f.freed.attributed_bytes, AllocatedBytes(expect));
    assert_eq!(f.freed.entries_removed, 9);
    assert_eq!(
        ops.calls(Op::Unlinkat),
        10,
        "no unlink may follow the ENOSPC"
    );
    assert!(exists(&fx.nm));
}

#[test]
fn subdir_reported_on_other_dev_or_other_mnt_id_should_be_skipped_CrossDevice_and_siblings_removed_When_report_dev_42_or_report_mnt_id_9(
) {
    for variant in ["dev", "mnt"] {
        let fx = Fx::new();
        let nm = &fx.nm;
        let sub = if variant == "dev" { "mnt" } else { "mnt2" };
        write_file(&nm.join("sibling1"), 8192);
        write_file(&nm.join("sibling2"), 8192);
        fs::create_dir_all(nm.join("plain/inner")).unwrap();
        write_file(&nm.join("plain/inner/deep"), 8192);
        write_file(&nm.join(sub).join("inside1"), 8192);
        write_file(&nm.join(sub).join("nested/inside2"), 8192);
        let sib_bytes = bytes_of(&nm.join("sibling1")) + bytes_of(&nm.join("sibling2"));
        let deep_bytes = bytes_of(&nm.join("plain/inner/deep"));
        let dirs_bytes = bytes_of(&nm.join("plain")) + bytes_of(&nm.join("plain/inner"));
        assert!(bytes_of(&nm.join(sub).join("inside1")) > 0);
        let mnt_before = {
            let mut m = BTreeMap::new();
            snap_into(&fx.base, &nm.join(sub), &[], &mut m);
            m
        };
        let mut pol = fx.policy();
        let ops = if variant == "dev" {
            FaultDirOps::wrap(fx.ops())
                .report_dev(At::Name(OsString::from(sub)), 42)
                .build()
        } else {
            pol.root_mnt = Some(5);
            FaultDirOps::wrap(fx.ops())
                .report_mnt_id(At::Any, 5)
                .report_mnt_id(At::Name(OsString::from(sub)), 9)
                .build()
        };

        let r = fx.run(&ops, &pol, &AbortFlag::new());

        let f = r.unwrap_err();
        assert_eq!(f.error, DeleteError::CrossDevice, "variant {variant}");
        assert!(
            !exists(&nm.join("sibling1")) && !exists(&nm.join("sibling2")),
            "siblings must go"
        );
        assert!(!exists(&nm.join("plain")), "same-device subtree must go");
        let mut after = BTreeMap::new();
        snap_into(&fx.base, &nm.join(sub), &[], &mut after);
        assert_eq!(
            after, mnt_before,
            "the other-device subtree was entered ({variant})"
        );
        let lo = sib_bytes + deep_bytes;
        let hi = lo + dirs_bytes;
        assert!(
            (lo..=hi).contains(&f.freed.attributed_bytes.0),
            "freed must cover exactly the same-device files ({lo}..={hi}); bytes of the other-device subtree must not be counted: {f:?}"
        );
    }
}

#[test]
fn ten_thousand_level_nesting_should_complete_without_stack_overflow_When_deleting() {
    if !cfg!(target_os = "linux") {
        eprintln!("SKIP: 10 000 nested levels take about 16 minutes on APFS (965 s measured on a macos-15 runner): Linux only");
        return;
    }
    let fx = Fx::new();
    build_chain(&fx.nm, 10_000);
    let nm = fx.nm.clone();
    struct Unwind(PathBuf);
    impl Drop for Unwind {
        fn drop(&mut self) {
            unwind_chain(&self.0);
        }
    }
    let _guard = Unwind(nm);
    let mut pol = fx.policy();
    pol.max_depth = 100_000;
    let ops = fx.ops();
    let abort = AbortFlag::new();
    // 256 KiB of stack: a recursive engine dies long before 10 000 levels
    let r = std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn_scoped(s, || fx.run(&ops, &pol, &abort))
            .unwrap()
            .join()
            .unwrap()
    });
    assert!(r.is_ok(), "{:?}", r.map(|_| ()));
    assert!(!exists(&fx.nm));
    assert!(exists(&fx.app.join("package.json")));
}

#[cfg(target_os = "linux")]
#[test]
fn fifo_socket_chardev_nodes_should_be_unlinked_without_opening_them_When_in_tree() {
    use rustix::fs::{makedev, mknodat, FileType, Mode, CWD};
    let fx = Fx::new();
    let nm = fx.nm.clone();
    fs::create_dir_all(nm.join("sub")).unwrap();
    for dir in [nm.clone(), nm.join("sub")] {
        mknodat(
            CWD,
            dir.join("fifo"),
            FileType::Fifo,
            Mode::from_bits_truncate(0o600),
            0,
        )
        .unwrap();
        // keep the listeners bound so the socket files stay real sockets until the engine unlinks them
    }
    let l1 = std::os::unix::net::UnixListener::bind(nm.join("sock")).unwrap();
    let l2 = std::os::unix::net::UnixListener::bind(nm.join("sub/sock")).unwrap();
    // device nodes need CAP_MKNOD; a normal user gets EPERM and the case runs with FIFOs and sockets only
    let mut devices = 0;
    for (name, ty) in [
        ("chr", FileType::CharacterDevice),
        ("blk", FileType::BlockDevice),
    ] {
        if mknodat(
            CWD,
            nm.join(name),
            ty,
            Mode::from_bits_truncate(0o600),
            makedev(1, 3),
        )
        .is_ok()
        {
            devices += 1;
        }
    }
    eprintln!("device nodes created: {devices}");
    let before = fx.snap_except(&[&fx.nm]);
    let base = fx.base.clone();
    let pol = fx.policy();
    let expect = ident(&fx.nm);
    let r = with_timeout(60, move || {
        let ops = RealDirOps::confined_to(&base);
        run_at(
            &ops,
            &base,
            "home/code/app",
            "node_modules",
            expect,
            &pol,
            &AbortFlag::new(),
        )
    });
    drop((l1, l2));
    assert!(r.is_ok(), "{r:?}");
    assert!(!exists(&fx.nm), "special files must be unlinked");
    assert_eq!(fx.snap_except(&[&fx.nm]), before);
}

#[test]
fn hardlink_pair_should_count_blocks_once_and_inodes_freed_2_entries_3_When_two_links_one_inode() {
    let fx = Fx::new();
    write_file(&fx.nm.join("a"), 65_536);
    fs::hard_link(fx.nm.join("a"), fx.nm.join("b")).unwrap();
    let file_bytes = bytes_of(&fx.nm.join("a"));
    let dir_bytes = bytes_of(&fx.nm);
    assert!(file_bytes > 0);
    assert_eq!(fs::symlink_metadata(fx.nm.join("a")).unwrap().nlink(), 2);

    let r = fx.run(&fx.fault(), &fx.policy(), &AbortFlag::new());

    let freed = r.unwrap();
    assert_eq!(
        freed.attributed_bytes,
        AllocatedBytes(file_bytes + dir_bytes),
        "the shared inode counts once; double counting would be {}",
        2 * file_bytes + dir_bytes
    );
    assert_eq!(freed.entries_removed, 3, "a, b and node_modules");
    assert_eq!(
        freed.inodes_freed, 2,
        "the shared file inode once, plus node_modules"
    );
    assert!(!exists(&fx.nm));
}

#[test]
fn read_only_directories_should_be_chmodded_on_verified_fd_and_removed_When_mode_0555() {
    let fx = Fx::new();
    let nm = &fx.nm;
    write_text(&nm.join("ro0/f"), "f");
    write_text(&nm.join("ro0/ro1/g"), "g");
    write_text(&nm.join("ro0/ro1/ro2/h"), "h");
    // an outside read-only directory that a link inside the tree points at must keep its mode
    write_text(&fx.outside.join("ro/inner"), "i");
    fs::set_permissions(fx.outside.join("ro"), fs::Permissions::from_mode(0o555)).unwrap();
    symlink(fx.outside.join("ro"), nm.join("ro0/link_to_outside_ro")).unwrap();
    for d in ["ro0/ro1/ro2", "ro0/ro1", "ro0"] {
        fs::set_permissions(nm.join(d), fs::Permissions::from_mode(0o555)).unwrap();
    }
    let app_mode = fs::symlink_metadata(&fx.app).unwrap().mode();
    let mut pol = fx.policy();
    pol.restore_owner_write = true;
    let ops = fx.fault();

    let r = fx.run(&ops, &pol, &AbortFlag::new());

    assert!(r.is_ok(), "{r:?}");
    assert!(!exists(&fx.nm));
    assert!(
        ops.calls(Op::Fchmod) >= 3,
        "each read-only directory needs a fchmod on its own fd"
    );
    assert_eq!(
        fs::symlink_metadata(fx.outside.join("ro")).unwrap().mode() & 0o7777,
        0o555,
        "a chmod landed on a path outside the tree"
    );
    assert_eq!(
        fs::read_to_string(fx.outside.join("ro/inner")).unwrap(),
        "i"
    );
    assert_eq!(fs::symlink_metadata(&fx.app).unwrap().mode(), app_mode);
}

#[test]
fn names_with_newlines_and_invalid_utf8_should_be_removed_exactly_When_in_tree() {
    let fx = Fx::new();
    let nm = &fx.nm;
    let weird: Vec<OsString> = vec![
        OsString::from("a\nb"),
        OsString::from("\n"),
        OsString::from("trailing\n"),
        OsString::from("-rf"),
        OsString::from("--"),
        OsString::from(" "),
        OsString::from("tab\tname"),
        OsString::from_vec(b"bad\xff\xfename".to_vec()),
        OsString::from_vec(b"\xc3\x28".to_vec()),
        OsString::from_vec(b"*".to_vec()),
    ];
    // APFS refuses a name that is not valid UTF-8, so those names exist on Linux only
    let on_linux = cfg!(target_os = "linux");
    let weird: Vec<OsString> = weird
        .into_iter()
        .filter(|n| on_linux || n.to_str().is_some())
        .collect();
    let inner: &[u8] = if on_linux { b"in\nner\xff" } else { b"in\nner" };
    for (i, n) in weird.iter().enumerate() {
        if i % 2 == 0 {
            fs::write(nm.join(n), b"data").unwrap();
        } else {
            fs::create_dir(nm.join(n)).unwrap();
            fs::write(nm.join(n).join(OsStr::from_bytes(inner)), b"x").unwrap();
        }
    }
    // a decoy next to the tree whose name equals a lossy rendering of one of the names inside it
    write_text(&fx.app.join("bad\u{FFFD}\u{FFFD}name"), "decoy");
    let before = fx.snap_except(&[&fx.nm]);

    let r = fx.run(&fx.fault(), &fx.policy(), &AbortFlag::new());

    assert!(r.is_ok(), "{r:?}");
    assert!(
        !exists(&fx.nm),
        "an entry survived: names were altered on the way to unlinkat"
    );
    assert_eq!(fx.snap_except(&[&fx.nm]), before);
}

#[test]
fn enotempty_from_concurrent_writer_should_map_to_Busy_without_retry_loop_When_before_unlinkat_creates_file(
) {
    let fx = Fx::new();
    write_text(&fx.nm.join("sub/f"), "f");
    write_text(&fx.nm.join("other"), "o");
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let target = PathBuf::from("home/code/app/node_modules/sub");
    let ops = FaultDirOps::wrap(fx.ops())
        .hook(Event::BeforeUnlinkat, move |ctx: &HookCtx| {
            if ctx.rel_path == target {
                h.fetch_add(1, Ordering::SeqCst);
                fs::write(ctx.base_abs.join(&ctx.rel_path).join("intruder"), b"w").unwrap();
            }
        })
        .build();

    let r = fx.run(&ops, &fx.policy(), &AbortFlag::new());

    let f = r.unwrap_err();
    assert_eq!(
        f.error,
        DeleteError::Busy,
        "ENOTEMPTY must be Busy, not Io(ENOTEMPTY)"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the rmdir of the busy directory was retried"
    );
    assert!(
        exists(&fx.nm.join("sub/intruder")),
        "the concurrent writer's file must survive"
    );
}

#[test]
fn enoent_during_delete_should_be_treated_as_removed_and_idempotent_on_second_call_When_entry_vanishes(
) {
    // (a) a file vanishes between listing and unlinkat
    {
        let fx = Fx::new();
        for n in ["x", "y", "z"] {
            write_text(&fx.nm.join(n), n);
        }
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let target = PathBuf::from("home/code/app/node_modules/y");
        let ops = FaultDirOps::wrap(fx.ops())
            .hook(Event::BeforeUnlinkat, move |ctx: &HookCtx| {
                if ctx.rel_path == target && !flag.swap(true, Ordering::SeqCst) {
                    fs::remove_file(ctx.base_abs.join(&ctx.rel_path)).unwrap();
                }
            })
            .build();
        let r = fx.run(&ops, &fx.policy(), &AbortFlag::new());
        assert!(done.load(Ordering::SeqCst));
        assert!(
            r.is_ok(),
            "ENOENT on unlinkat is already-removed, not a failure: {r:?}"
        );
        assert!(!exists(&fx.nm));
    }
    // (b) a whole subdirectory vanishes after it was opened
    {
        let fx = Fx::new();
        write_text(&fx.nm.join("sub/f"), "f");
        write_text(&fx.nm.join("keep"), "k");
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let target = PathBuf::from("home/code/app/node_modules/sub");
        let ops = FaultDirOps::wrap(fx.ops())
            .hook(Event::AfterOpenDir, move |ctx: &HookCtx| {
                if ctx.rel_path == target && !flag.swap(true, Ordering::SeqCst) {
                    fs::remove_dir_all(ctx.base_abs.join(&ctx.rel_path)).unwrap();
                }
            })
            .build();
        let r = fx.run(&ops, &fx.policy(), &AbortFlag::new());
        assert!(done.load(Ordering::SeqCst));
        assert!(r.is_ok(), "{r:?}");
        assert!(!exists(&fx.nm));
    }
    // (c) the second call on an already deleted target is Ok and frees nothing
    let fx = Fx::new();
    fx.fill_flat(3);
    let id = ident(&fx.nm);
    let ops = fx.fault();
    let pol = fx.policy();
    let first = run_at(
        &ops,
        &fx.base,
        "home/code/app",
        "node_modules",
        id,
        &pol,
        &AbortFlag::new(),
    );
    assert!(first.is_ok(), "{first:?}");
    let second = run_at(
        &ops,
        &fx.base,
        "home/code/app",
        "node_modules",
        id,
        &pol,
        &AbortFlag::new(),
    );
    let freed = second.unwrap();
    assert_eq!(freed.entries_removed, 0);
    assert_eq!(freed.attributed_bytes, AllocatedBytes(0));
}

#[test]
fn verified_parent_for_dir_X_with_target_under_dir_Y_should_return_OutsideScope_even_when_Y_has_same_name_When_paired(
) {
    let fx = Fx::new();
    let (x, y) = (fx.code.join("X"), fx.code.join("Y"));
    for d in [&x, &y] {
        write_text(&d.join("package.json"), "{}");
        write_text(&d.join("node_modules/pkg/index.js"), "x");
    }
    let before = fx.snap();
    let ops = fx.fault();
    let root = open_verified_root(&ops, &fx.base, ident(&fx.base)).unwrap();
    let parent_x = open_parent(&ops, &root, Path::new("home/code/X")).unwrap();
    let pol_y = fx.policy_for(&y.join("node_modules"));
    for expect in [
        ident(&x.join("node_modules")),
        ident(&y.join("node_modules")),
    ] {
        let r = delete_tree(
            &ops,
            &parent_x,
            OsStr::new("node_modules"),
            expect,
            &pol_y,
            &AbortFlag::new(),
            &ApplyToken::for_tests(),
        );
        assert_eq!(err_kind(&r), Some(DeleteError::OutsideScope), "{r:?}");
        assert_eq!(fx.snap(), before, "the pairing mismatch deleted something");
    }
    assert_eq!(ops.calls(Op::Unlinkat), 0);
    // control: the matching pair deletes X and only X
    let pol_x = fx.policy_for(&x.join("node_modules"));
    let r = delete_tree(
        &ops,
        &parent_x,
        OsStr::new("node_modules"),
        ident(&x.join("node_modules")),
        &pol_x,
        &AbortFlag::new(),
        &ApplyToken::for_tests(),
    );
    assert!(r.is_ok(), "{r:?}");
    assert!(!exists(&x.join("node_modules")));
    assert!(exists(&y.join("node_modules/pkg/index.js")));
}

#[test]
fn missing_sibling_marker_should_return_OutsideScope_When_require_sibling_any_not_found() {
    let fx = Fx::new();
    fs::remove_file(fx.app.join("package.json")).unwrap();
    write_text(&fx.nm.join("pkg/index.js"), "x");
    let before = fx.snap();
    let mut pol = fx.policy();
    pol.require_sibling_any = [
        "package.json",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
    ]
    .map(OsString::from)
    .to_vec();
    let ops = fx.fault();

    let r = fx.run(&ops, &pol, &AbortFlag::new());

    assert_eq!(err_kind(&r), Some(DeleteError::OutsideScope), "{r:?}");
    assert_eq!(ops.calls(Op::Unlinkat), 0);
    assert_eq!(fx.snap(), before, "a tree without any marker was touched");

    // "any" means any one of the markers is enough
    write_text(&fx.app.join("yarn.lock"), "");
    let r = fx.run(&fx.fault(), &pol, &AbortFlag::new());
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn fixture_hash_before_and_after_should_match_When_delete_tree_refused_by_validate() {
    let fx = Fx::new();
    let h = &fx.home;
    write_text(&h.join("code/myrepo/.git/HEAD"), "ref");
    write_text(&h.join("code/myrepo/src/main.rs"), "fn main(){}");
    write_text(&h.join("code/myrepo/package.json"), "{}");
    write_text(&h.join(".npm/_cacache/content/x"), "x");
    write_text(&fx.nm.join("pkg/index.js"), "x");
    let before = fx.snap();
    // (parent rel, name, allowed): every one of these is a floor or scope violation of rule F
    let exact = |rel: &str| AllowedTarget::Exact(fx.base.join(rel));
    let within = |n: NamePattern| AllowedTarget::Within {
        root: fx.code.clone(),
        name: n,
    };
    let cases: Vec<(&str, &str, AllowedTarget)> = vec![
        ("home", ".ssh", exact("home/.ssh")),
        ("home", "Documents", exact("home/Documents")),
        ("home", ".npm", exact("home/.npm")),
        ("home", "code", exact("home/code")),
        ("", "home", exact("home")),
        ("home/code", "myrepo", within(NamePattern::NodeModules)),
        ("home/code/myrepo", ".git", within(NamePattern::NodeModules)),
        ("home/code/myrepo", "src", within(NamePattern::NodeModules)),
        (
            "home/code/app",
            "node_modules",
            within(NamePattern::CargoTarget),
        ),
        ("home/code/app", "node_modules", exact("home/.npm/_cacache")),
    ];
    for (rel, name, allowed) in cases {
        let ops = fx.fault();
        let target = fx.base.join(rel).join(name);
        let mut pol = fx.policy_for(&target);
        pol.allowed = allowed;
        let r = run_at(
            &ops,
            &fx.base,
            rel,
            name,
            ident(&target),
            &pol,
            &AbortFlag::new(),
        );
        assert!(
            err_kind(&r).as_ref().is_some_and(is_refusal),
            "{rel}/{name} must be refused by rule F: {r:?}"
        );
        assert_eq!(ops.calls(Op::Unlinkat), 0, "{rel}/{name}");
        assert_eq!(ops.calls(Op::Fchmod), 0, "{rel}/{name}");
        assert_eq!(fx.snap(), before, "{rel}/{name} left a partial deletion");
    }
    // control: the legitimate target of the same fixture goes
    let r = fx.run(&fx.fault(), &fx.policy(), &AbortFlag::new());
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn identity_mismatch_between_expect_and_fstat_should_return_IdentityMismatch_and_delete_nothing_When_dir_replaced(
) {
    let fx = Fx::new();
    write_text(&fx.nm.join("old/f"), "old");
    let old_id = ident(&fx.nm);
    // replace the directory behind the caller's back: same name, new inode
    fs::rename(&fx.nm, fx.app.join("node_modules.old")).unwrap();
    write_text(&fx.nm.join("new/f"), "new");
    assert_ne!(ident(&fx.nm), old_id);
    let before = fx.snap();
    let ops = fx.fault();
    let pol = fx.policy();

    let r = run_at(
        &ops,
        &fx.base,
        "home/code/app",
        "node_modules",
        old_id,
        &pol,
        &AbortFlag::new(),
    );
    assert_eq!(err_kind(&r), Some(DeleteError::IdentityMismatch), "{r:?}");
    let wrong_dev = Identity {
        dev: old_id.dev + 1,
        ino: ident(&fx.nm).ino,
    };
    let r = run_at(
        &ops,
        &fx.base,
        "home/code/app",
        "node_modules",
        wrong_dev,
        &pol,
        &AbortFlag::new(),
    );
    assert_eq!(err_kind(&r), Some(DeleteError::IdentityMismatch), "{r:?}");
    assert_eq!(ops.calls(Op::Unlinkat), 0);
    assert_eq!(fx.snap(), before, "the replacement directory was deleted");

    // control: the right identity deletes
    let r = run_at(
        &ops,
        &fx.base,
        "home/code/app",
        "node_modules",
        ident(&fx.nm),
        &pol,
        &AbortFlag::new(),
    );
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn hardlink_to_file_outside_tree_should_unlink_only_the_in_tree_name_When_nlink_gt_1() {
    let fx = Fx::new();
    let outside_file = fx.outside.join("shared.bin");
    write_file(&outside_file, 65_536);
    fs::hard_link(&outside_file, fx.nm.join("linked")).unwrap();
    let dir_bytes = bytes_of(&fx.nm);
    let content = fs::read(&outside_file).unwrap();
    assert_eq!(fs::symlink_metadata(&outside_file).unwrap().nlink(), 2);

    let r = fx.run(&fx.fault(), &fx.policy(), &AbortFlag::new());

    let freed = r.unwrap();
    assert!(!exists(&fx.nm));
    assert_eq!(
        fs::read(&outside_file).unwrap(),
        content,
        "outside link content changed"
    );
    assert_eq!(fs::symlink_metadata(&outside_file).unwrap().nlink(), 1);
    assert_eq!(
        freed.attributed_bytes,
        AllocatedBytes(dir_bytes),
        "the inode lives on through the outside link, so its bytes were not freed"
    );
    assert_eq!(freed.entries_removed, 2);
    assert_eq!(freed.inodes_freed, 1, "only node_modules itself was freed");
}

#[test]
fn directory_mode_000_unreadable_should_return_Io_EACCES_with_partial_freed_and_continue_siblings_When_in_tree(
) {
    if rustix::process::geteuid().is_root() {
        eprintln!("running as root: mode 000 does not stop root, case not applicable");
        return;
    }
    let fx = Fx::new();
    let nm = &fx.nm;
    let mut sibling_files = Vec::new();
    for i in 0..3 {
        let p = nm.join(format!("s{i}"));
        write_file(&p, 8192);
        sibling_files.push(p);
    }
    let in_ok = nm.join("ok/inner");
    write_file(&in_ok, 8192);
    sibling_files.push(in_ok);
    write_text(&nm.join("locked/secret"), "s");
    let sizes: u64 = sibling_files.iter().map(|p| bytes_of(p)).sum();
    assert!(sizes > 0);
    fs::set_permissions(nm.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    let ops = fx.fault();

    let r = fx.run(&ops, &fx.policy(), &AbortFlag::new());

    fs::set_permissions(nm.join("locked"), fs::Permissions::from_mode(0o700)).unwrap();
    let f = r.unwrap_err();
    assert_eq!(f.error, DeleteError::Io(libc::EACCES), "{f:?}");
    assert!(
        sibling_files.iter().all(|p| !exists(p)),
        "siblings must still be removed"
    );
    assert!(!exists(&nm.join("ok")));
    assert_eq!(fs::read_to_string(nm.join("locked/secret")).unwrap(), "s");
    assert!(
        f.freed.attributed_bytes.0 >= sizes,
        "partial freed lost: {f:?}"
    );
    assert!(f.freed.entries_removed >= 5, "{f:?}");
}

#[test]
fn fd_count_should_return_to_baseline_When_delete_tree_returns_ok_or_err() {
    let fx = Fx::new();
    for i in 0..30 {
        write_text(&fx.nm.join(format!("d{}/e{}/f{i}", i % 3, i % 2)), "x");
    }
    let pol = fx.policy();
    let baseline = fd_count();

    // Ok path
    {
        let ops = fx.fault();
        let r = fx.run(&ops, &pol, &AbortFlag::new());
        assert!(r.is_ok(), "{r:?}");
    }
    assert_eq!(fd_count(), baseline, "fds leaked on the Ok path");

    // Err path: ENOSPC part way through a second tree
    fs::create_dir_all(&fx.nm).unwrap();
    for i in 0..30 {
        write_text(&fx.nm.join(format!("d{}/e{}/f{i}", i % 3, i % 2)), "x");
    }
    let id2 = ident(&fx.nm);
    let baseline = fd_count();
    {
        let ops = FaultDirOps::wrap(fx.ops())
            .enospc_on_nth_unlinkat(7)
            .build();
        let r = run_at(
            &ops,
            &fx.base,
            "home/code/app",
            "node_modules",
            id2,
            &pol,
            &AbortFlag::new(),
        );
        assert_eq!(err_kind(&r), Some(DeleteError::NoSpace), "{r:?}");
    }
    assert_eq!(fd_count(), baseline, "fds leaked on the Err path");

    // Err path: a refusal before anything is deleted
    {
        let ops = fx.fault();
        let mut bad = pol.clone();
        bad.allowed = AllowedTarget::Exact(fx.base.join("elsewhere"));
        let r = run_at(
            &ops,
            &fx.base,
            "home/code/app",
            "node_modules",
            id2,
            &bad,
            &AbortFlag::new(),
        );
        assert!(r.is_err(), "{r:?}");
    }
    assert_eq!(fd_count(), baseline, "fds leaked on the refusal path");
}

#[allow(dead_code)]
fn _types_used(_: &VerifiedRoot<()>, _: &VerifiedParent<()>) {}

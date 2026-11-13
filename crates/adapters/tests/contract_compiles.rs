#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::ignored_unit_patterns,
    clippy::no_effect_underscore_binding,
    clippy::type_complexity
)]
//! Compiles every signature of the frozen `contract` block (see `dunnage_adapters::delete`) and
//! exercises the `FaultDirOps` seam against real tempdirs.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use dunnage_adapters::testing::{At, Event, FaultDirOps, HookCtx, Op};
use dunnage_adapters::{
    delete_tree, open_parent, open_verified_root, DirOps, RealDirOps, VerifiedParent, VerifiedRoot,
    DELETE_BATCH,
};
use dunnage_core::{
    first_line_capped, AbortFlag, AllocatedBytes, AllowedTarget, ApparentBytes, ApplyToken,
    DeleteError, DeleteFailure, DeletePolicy, DirEntryInfo, FileKind, Freed, Identity, NamePattern,
    RootDev, StatInfo, CMD_MSG_MAX,
};

struct Dummy;

fn stat() -> StatInfo {
    StatInfo {
        ident: Identity { dev: 1, ino: 2 },
        kind: FileKind::Dir,
        mode: 0o755,
        nlink: 2,
        blocks: 8,
        size: 4096,
        mtime_secs: 0,
        mtime_nsec: 0,
        mnt_id: None,
        btime: None,
    }
}

impl DirOps for Dummy {
    type Dir = ();
    type ReadDir = std::vec::IntoIter<io::Result<DirEntryInfo>>;
    fn open_root(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn open_dir(&self, _: &(), _: &OsStr) -> io::Result<()> {
        Ok(())
    }
    fn fstat(&self, _: &()) -> io::Result<StatInfo> {
        Ok(stat())
    }
    fn stat_at(&self, _: &(), _: &OsStr) -> io::Result<StatInfo> {
        Ok(stat())
    }
    fn read_dir(&self, _: &()) -> io::Result<Self::ReadDir> {
        Ok(vec![Ok(DirEntryInfo {
            name: OsString::from("x"),
            kind: FileKind::File,
            ino: 3,
        })]
        .into_iter())
    }
    fn unlinkat(&self, _: &(), _: &OsStr, _: bool) -> io::Result<()> {
        Ok(())
    }
    fn fchmod(&self, _: &(), _: u32) -> io::Result<()> {
        Ok(())
    }
}

fn policy() -> DeletePolicy {
    DeletePolicy {
        root_dev: RootDev(1),
        root_mnt: None,
        home: PathBuf::from("/h"),
        home_id: Identity { dev: 1, ino: 1 },
        scan_roots: vec![PathBuf::from("/h/code")],
        allowed: AllowedTarget::Within {
            root: PathBuf::from("/h/code"),
            name: NamePattern::NodeModules,
        },
        target: PathBuf::from("/h/code/node_modules"),
        require_sibling_any: vec![OsString::from("package.json")],
        max_depth: 64,
        restore_owner_write: false,
    }
}

#[test]
fn dummy_dirops_should_implement_and_call_every_contract_item_when_compiled() {
    let ops = Dummy;
    let id = Identity { dev: 1, ino: 2 };
    let root: VerifiedRoot<()> = open_verified_root(&ops, Path::new("/x"), id).unwrap();
    assert_eq!(root.abs_path(), Path::new("/x"));
    let parent: VerifiedParent<()> = open_parent(&ops, &root, Path::new("a/b")).unwrap();
    assert_eq!(parent.abs_path(), Path::new("/x/a/b"));
    let () = *parent.dir();
    assert_eq!(ops.fstat(&()).unwrap(), stat());
    assert_eq!(ops.stat_at(&(), OsStr::new("f")).unwrap().blocks, 8);
    assert_eq!(ops.read_dir(&()).unwrap().count(), 1);
    ops.unlinkat(&(), OsStr::new("f"), false).unwrap();
    ops.fchmod(&(), 0o700).unwrap();
    assert!(matches!(
        open_verified_root(&ops, Path::new("/x"), Identity { dev: 9, ino: 9 }),
        Err(DeleteError::IdentityMismatch)
    ));
    assert_eq!(DELETE_BATCH, 1024);
    assert_eq!(CMD_MSG_MAX, 200);
    assert_eq!(first_line_capped("a\nb"), "a");

    // delete_tree has exactly this signature (not called: the body is the frozen stub).
    let _f: fn(
        &Dummy,
        &VerifiedParent<()>,
        &OsStr,
        Identity,
        &DeletePolicy,
        &AbortFlag,
        &ApplyToken,
    ) -> Result<Freed, DeleteFailure> = delete_tree::<Dummy>;

    // data derives the tests rely on
    assert_eq!(AllocatedBytes(1), AllocatedBytes(1));
    assert_eq!(ApparentBytes::default(), ApparentBytes(0));
    assert_eq!(policy().clone(), policy());
    let _ = format!(
        "{:?}",
        DeleteError::CommandFailed {
            status: 1,
            message: String::new()
        }
    );
}

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().canonicalize().unwrap();
    (t, p)
}

#[test]
fn fault_dirops_should_follow_the_builder_semantics_over_a_real_tempdir() {
    let (_t, p) = tmp();
    std::fs::create_dir_all(p.join("a/b")).unwrap();
    std::fs::create_dir_all(p.join("sub")).unwrap();
    std::fs::create_dir_all(p.join("mnt")).unwrap();
    for i in 0..6 {
        std::fs::write(p.join(format!("sub/f{i}")), b"").unwrap();
    }
    let hooked = Arc::new(AtomicUsize::new(0));
    let h2 = hooked.clone();
    let seen: Arc<std::sync::Mutex<Vec<(PathBuf, Event)>>> = Arc::default();
    let s2 = seen.clone();
    let ops = FaultDirOps::wrap(RealDirOps::confined_to(&p))
        .enospc_on_nth_unlinkat(5)
        .errno_on(Op::OpenDir, At::Rel("a/b".into()), libc::EACCES)
        .report_dev(At::Rel("sub".into()), 42)
        .report_mnt_id(At::Name("mnt".into()), 7)
        .hook(Event::AfterOpenDir, move |ctx: &HookCtx| {
            h2.fetch_add(1, Ordering::SeqCst);
            s2.lock().unwrap().push((ctx.rel_path.clone(), ctx.event));
            assert!(ctx.base_abs.is_absolute());
        })
        .build();
    let root = open_verified_root(&ops, &p, Identity::of(&std::fs::metadata(&p).unwrap())).unwrap();
    // errno_on(OpenDir, a/b): a opens, a/b fails with EACCES
    let a = ops.open_dir(root.dir(), OsStr::new("a")).unwrap();
    let e = ops.open_dir(&a, OsStr::new("b")).err().unwrap();
    assert_eq!(e.raw_os_error(), Some(libc::EACCES));
    assert_eq!(
        open_parent(&ops, &root, Path::new("a/b")).err(),
        Some(DeleteError::Io(libc::EACCES))
    );
    // report_dev on fstat of sub, report_mnt_id on stat_at of mnt
    let sub = ops.open_dir(root.dir(), OsStr::new("sub")).unwrap();
    assert_eq!(ops.fstat(&sub).unwrap().ident.dev, 42);
    assert_eq!(
        ops.stat_at(root.dir(), OsStr::new("sub"))
            .unwrap()
            .ident
            .dev,
        42
    );
    assert_eq!(
        ops.stat_at(root.dir(), OsStr::new("mnt")).unwrap().mnt_id,
        Some(7)
    );
    // hook ran for the open_dir calls that succeeded
    assert!(hooked.load(Ordering::SeqCst) >= 2);
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .any(|(r, e)| r == Path::new("sub") && *e == Event::AfterOpenDir));
    // enospc on exactly the 5th unlinkat
    for i in 0..4 {
        ops.unlinkat(&sub, OsStr::new(&format!("f{i}")), false)
            .unwrap();
    }
    let e = ops.unlinkat(&sub, OsStr::new("f4"), false).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOSPC));
    assert!(p.join("sub/f4").exists());
    ops.unlinkat(&sub, OsStr::new("f4"), false).unwrap();
    assert_eq!(ops.calls(Op::Unlinkat), 6);
    assert_eq!(ops.calls(Op::OpenRoot), 1);
    assert!(ops.calls(Op::Fstat) >= 2 && ops.calls(Op::StatAt) == 2);
}

#[test]
fn open_parent_should_not_open_anything_for_an_unclean_rel_path() {
    let (_t, p) = tmp();
    let ops = FaultDirOps::wrap(RealDirOps::confined_to(&p)).build();
    let root = open_verified_root(&ops, &p, Identity::of(&std::fs::metadata(&p).unwrap())).unwrap();
    assert_eq!(
        open_parent(&ops, &root, Path::new("../.ssh")).err(),
        Some(DeleteError::OutsideScope)
    );
    assert_eq!(ops.calls(Op::OpenDir), 0);
}

#[test]
fn hooks_and_read_dir_faults_should_apply_to_the_iterator_creating_call() {
    let (_t, p) = tmp();
    std::fs::create_dir_all(p.join("d")).unwrap();
    let before = Arc::new(AtomicUsize::new(0));
    let b2 = before.clone();
    let victim = p.join("d/new");
    let ops = FaultDirOps::wrap(RealDirOps::confined_to(&p))
        .hook(Event::BeforeReadDir, move |_| {
            b2.fetch_add(1, Ordering::SeqCst);
        })
        .hook(Event::BeforeUnlinkat, {
            let victim = victim.clone();
            move |_| {
                let _ = std::fs::write(&victim, b"");
            }
        })
        .errno_on(Op::Unlinkat, At::Name("locked".into()), libc::ENOTEMPTY)
        .build();
    let root = open_verified_root(&ops, &p, Identity::of(&std::fs::metadata(&p).unwrap())).unwrap();
    let d = ops.open_dir(root.dir(), OsStr::new("d")).unwrap();
    assert_eq!(ops.read_dir(&d).unwrap().count(), 0);
    assert_eq!(before.load(Ordering::SeqCst), 1);
    assert_eq!(ops.calls(Op::ReadDir), 1);
    let e = ops.unlinkat(&d, OsStr::new("locked"), true).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOTEMPTY));
    assert!(victim.exists());
}

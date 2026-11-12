#![allow(clippy::unwrap_used, clippy::expect_used)]
//! RealDirOps against real tempdirs only; every ops value is confined to the tempdir.

use std::ffi::OsStr;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::Path;

use dunnage_adapters::dirops::is_scope_violation;
use dunnage_adapters::{open_parent, open_verified_root, DirOps, RealDirOps};
use dunnage_core::{DeleteError, FileKind, Identity};

fn tmp() -> (tempfile::TempDir, std::path::PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().canonicalize().unwrap();
    (t, p)
}

fn ident(p: &Path) -> Identity {
    Identity::of(&std::fs::metadata(p).unwrap())
}

#[test]
fn open_parent_should_walk_nested_dirs_and_empty_rel_should_reopen_root() {
    let (_t, p) = tmp();
    std::fs::create_dir_all(p.join("a/b")).unwrap();
    let ops = RealDirOps::confined_to(&p);
    let root = open_verified_root(&ops, &p, ident(&p)).unwrap();
    let parent = open_parent(&ops, &root, Path::new("a/b")).unwrap();
    assert_eq!(parent.abs_path(), p.join("a/b"));
    assert_eq!(
        ops.fstat(parent.dir()).unwrap().ident,
        ident(&p.join("a/b"))
    );
    let same = open_parent(&ops, &root, Path::new("")).unwrap();
    assert_eq!(same.abs_path(), root.abs_path());
}

#[test]
fn open_parent_should_refuse_unclean_rel_paths_as_outside_scope() {
    let (_t, p) = tmp();
    std::fs::create_dir_all(p.join("a/b")).unwrap();
    let ops = RealDirOps::confined_to(&p);
    let root = open_verified_root(&ops, &p, ident(&p)).unwrap();
    for bad in [
        "..", "../.ssh", "a/../b", "/etc", "a//b", "a/./b", "./a", "a/", "a/..",
    ] {
        let e = open_parent(&ops, &root, Path::new(bad)).err();
        assert_eq!(e, Some(DeleteError::OutsideScope), "{bad}");
    }
}

#[test]
fn open_parent_should_report_symlink_and_non_directory_without_following() {
    let (_t, p) = tmp();
    std::fs::create_dir_all(p.join("real")).unwrap();
    symlink(p.join("real"), p.join("link")).unwrap();
    std::fs::write(p.join("file"), b"x").unwrap();
    let ops = RealDirOps::confined_to(&p);
    let root = open_verified_root(&ops, &p, ident(&p)).unwrap();
    assert_eq!(
        open_parent(&ops, &root, Path::new("link")).err(),
        Some(DeleteError::Symlink)
    );
    assert_eq!(
        open_parent(&ops, &root, Path::new("file")).err(),
        Some(DeleteError::Symlink)
    );
    assert_eq!(
        open_parent(&ops, &root, Path::new("link/x")).err(),
        Some(DeleteError::Symlink)
    );
}

#[test]
fn open_verified_root_should_fail_with_identity_mismatch_and_on_a_symlinked_base() {
    let (_t, p) = tmp();
    std::fs::create_dir_all(p.join("d")).unwrap();
    symlink(p.join("d"), p.join("l")).unwrap();
    let ops = RealDirOps::confined_to(&p);
    let wrong = Identity { dev: 1, ino: 1 };
    assert!(matches!(
        open_verified_root(&ops, &p, wrong),
        Err(DeleteError::IdentityMismatch)
    ));
    assert!(matches!(
        open_verified_root(&ops, &p.join("l"), ident(&p.join("d"))),
        Err(DeleteError::Symlink)
    ));
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the kernel mount id (statx STATX_MNT_ID) exists on Linux only"
)]
fn stat_should_match_std_metadata_and_report_a_mount_id() {
    let (_t, p) = tmp();
    std::fs::write(p.join("f"), vec![1u8; 10_000]).unwrap();
    std::fs::hard_link(p.join("f"), p.join("g")).unwrap();
    symlink("f", p.join("s")).unwrap();
    let ops = RealDirOps::confined_to(&p);
    let root = open_verified_root(&ops, &p, ident(&p)).unwrap();
    let st = ops.stat_at(root.dir(), OsStr::new("f")).unwrap();
    let m = std::fs::metadata(p.join("f")).unwrap();
    assert_eq!((st.ident.dev, st.ident.ino), (m.dev(), m.ino()));
    assert_eq!((st.blocks, st.size, st.nlink), (m.blocks(), m.size(), 2));
    assert_eq!(st.kind, FileKind::File);
    assert_eq!(
        ops.stat_at(root.dir(), OsStr::new("s")).unwrap().kind,
        FileKind::Symlink
    );
    assert!(ops.fstat(root.dir()).unwrap().mnt_id.is_some());
}

#[test]
fn read_dir_should_stream_entries_without_dots_and_allow_unlink_while_iterating() {
    let (_t, p) = tmp();
    for i in 0..50 {
        std::fs::write(p.join(format!("f{i}")), b"").unwrap();
    }
    std::fs::create_dir(p.join("sub")).unwrap();
    let ops = RealDirOps::confined_to(&p);
    let root = open_verified_root(&ops, &p, ident(&p)).unwrap();
    let mut seen = 0;
    for e in ops.read_dir(root.dir()).unwrap() {
        let e = e.unwrap();
        assert_ne!(e.name, OsStr::new("."));
        assert_ne!(e.name, OsStr::new(".."));
        if e.kind == FileKind::File {
            ops.unlinkat(root.dir(), &e.name, false).unwrap();
        }
        seen += 1;
    }
    assert_eq!(seen, 51);
    let left: Vec<_> = ops
        .read_dir(root.dir())
        .unwrap()
        .map(|e| e.unwrap().name)
        .collect();
    assert_eq!(left, vec![OsStr::new("sub")]);
    ops.unlinkat(root.dir(), OsStr::new("sub"), true).unwrap();
}

#[test]
fn unlinkat_and_fchmod_should_refuse_handles_outside_the_confinement_root() {
    let (_t, p) = tmp();
    std::fs::create_dir_all(p.join("in")).unwrap();
    std::fs::write(p.join("outside-file"), b"keep").unwrap();
    let ops = RealDirOps::confined_to(&p.join("in"));
    let outer = ops.open_root(&p).unwrap();
    let e = ops
        .unlinkat(&outer, OsStr::new("outside-file"), false)
        .unwrap_err();
    assert!(is_scope_violation(&e));
    assert!(is_scope_violation(&ops.fchmod(&outer, 0o700).unwrap_err()));
    assert!(p.join("outside-file").exists());
    // read-only calls outside the root are allowed
    assert!(ops.stat_at(&outer, OsStr::new("outside-file")).is_ok());
    // inside is allowed
    std::fs::write(p.join("in/x"), b"").unwrap();
    let inner = ops.open_root(&p.join("in")).unwrap();
    ops.unlinkat(&inner, OsStr::new("x"), false).unwrap();
    assert!(!p.join("in/x").exists());
}

#[test]
fn names_that_escape_a_directory_should_be_refused() {
    let (_t, p) = tmp();
    std::fs::create_dir_all(p.join("in")).unwrap();
    std::fs::write(p.join("victim"), b"keep").unwrap();
    let ops = RealDirOps::confined_to(&p);
    let inner = ops.open_root(&p.join("in")).unwrap();
    for bad in ["..", "../victim", "a/b", ""] {
        assert!(
            is_scope_violation(&ops.unlinkat(&inner, OsStr::new(bad), false).unwrap_err()),
            "{bad}"
        );
    }
    assert!(is_scope_violation(
        &ops.open_dir(&inner, OsStr::new("..")).err().unwrap()
    ));
    assert!(p.join("victim").exists());
}

#[test]
fn a_non_canonical_confinement_root_should_refuse_everything_in_release() {
    if cfg!(debug_assertions) {
        let r = std::panic::catch_unwind(|| RealDirOps::confined_to(Path::new("relative/..")));
        assert!(r.is_err());
    } else {
        let (_t, p) = tmp();
        let ops = RealDirOps::confined_to(Path::new("relative/.."));
        assert!(is_scope_violation(&ops.open_root(&p).err().unwrap()));
    }
}

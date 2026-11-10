#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 2.1.4: the runtime scope guard of `RealDirOps::confined_to` works from an integration test, a
//! non-`cfg(test)` link of `adapters`, so it does not depend on `cfg(test)`. Tempdirs only.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use disk_cleanup_adapters::{delete_tree, open_parent, open_verified_root, RealDirOps};
use disk_cleanup_core::{
    AbortFlag, AllowedTarget, ApplyToken, DeleteError, DeletePolicy, Identity, NamePattern, RootDev,
};

fn ident(p: &Path) -> Identity {
    Identity::of(&fs::symlink_metadata(p).unwrap())
}

/// `<base>/inside/code` is the confinement root; `<base>/outside/app/node_modules` is the sibling tree.
struct World {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    confine: PathBuf,
    victim: PathBuf,
}

fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let confine = base.join("inside/code");
    let victim = base.join("outside/app/node_modules");
    fs::create_dir_all(&confine).unwrap();
    fs::create_dir_all(victim.join("pkg/lib")).unwrap();
    fs::write(victim.join("a.txt"), b"keep").unwrap();
    fs::write(victim.join("pkg/b.txt"), b"keep").unwrap();
    fs::write(victim.join("pkg/lib/c.txt"), b"keep").unwrap();
    fs::write(base.join("outside/app/package.json"), "{}").unwrap();
    World {
        _tmp: tmp,
        base,
        confine,
        victim,
    }
}

fn policy(w: &World) -> DeletePolicy {
    let home = w.base.join("outside");
    DeletePolicy {
        root_dev: RootDev(fs::metadata(&w.base).unwrap().dev()),
        root_mnt: None,
        home: home.clone(),
        home_id: ident(&home),
        scan_roots: vec![home.clone()],
        allowed: AllowedTarget::Within {
            root: home,
            name: NamePattern::NodeModules,
        },
        target: w.victim.clone(),
        require_sibling_any: vec![OsString::from("package.json")],
        max_depth: 64,
        restore_owner_write: false,
    }
}

fn snapshot(root: &Path) -> Vec<(PathBuf, u64)> {
    fn walk(p: &Path, out: &mut Vec<(PathBuf, u64)>) {
        let mut names: Vec<_> = fs::read_dir(p)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        names.sort();
        for n in names {
            let m = fs::symlink_metadata(&n).unwrap();
            out.push((n.clone(), m.ino()));
            if m.is_dir() {
                walk(&n, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

#[test]
fn delete_tree_should_return_OutsideScope_and_leave_the_sibling_untouched_When_confined_ops_target_a_sibling_directory(
) {
    let w = world();
    let before = snapshot(&w.base.join("outside"));
    // Handles to the sibling are opened by an unconfined value (reads only) so that the engine, not the
    // opening step, is what meets the guard.
    let open_ops = RealDirOps::unconfined(&ApplyToken::for_tests());
    let root = open_verified_root(&open_ops, &w.base, ident(&w.base)).unwrap();
    let parent = open_parent(&open_ops, &root, Path::new("outside/app")).unwrap();

    let confined = RealDirOps::confined_to(&w.confine);
    let err = delete_tree(
        &confined,
        &parent,
        "node_modules".as_ref(),
        ident(&w.victim),
        &policy(&w),
        &AbortFlag::new(),
        &ApplyToken::for_tests(),
    )
    .expect_err("a confined ops value must refuse a target outside its root");

    assert_eq!(err.error, DeleteError::OutsideScope);
    assert_eq!(snapshot(&w.base.join("outside")), before, "sibling changed");
}

#[test]
fn delete_tree_should_delete_When_the_same_target_is_inside_the_confinement_root() {
    let w = world();
    let root_ops = RealDirOps::confined_to(&w.base);
    let root = open_verified_root(&root_ops, &w.base, ident(&w.base)).unwrap();
    let parent = open_parent(&root_ops, &root, Path::new("outside/app")).unwrap();
    delete_tree(
        &root_ops,
        &parent,
        "node_modules".as_ref(),
        ident(&w.victim),
        &policy(&w),
        &AbortFlag::new(),
        &ApplyToken::for_tests(),
    )
    .unwrap();
    assert!(!w.victim.exists(), "the control must actually delete");
}

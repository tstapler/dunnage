#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Task 6.6.3f (review 2, N4): the engine ties `expect` to the identity the witness was minted for, so the
//! fresh identity of the directory cannot stand in for the planned one. Real tempdir fixtures only.

mod common;

use std::fs;
use std::path::Path;

use common::discovery::{fixture, roots_for, Fixture};
use dunnage_adapters::{delete_tree, open_parent, open_verified_root, DirOps, RealDirOps};
use dunnage_core::guard::{decide_path_common, Facts, GuardDecision};
use dunnage_core::plan::ItemIdentity;
use dunnage_core::{
    AbortFlag, Allowed, ApplyToken, DeleteError, DeletePolicy, FileKind, Identity, Kind, RootDev,
};

fn ident(p: &Path) -> Identity {
    Identity::of(&fs::symlink_metadata(p).unwrap())
}

/// A witness for `path` as the guard would mint it if it had seen `seen` (the planned identity).
fn witness_seen_as(f: &Fixture, path: &Path, seen: Identity) -> Allowed {
    let facts = Facts {
        kind: Kind::ExplorerSelection,
        path: path.to_path_buf(),
        ident: seen,
        is_symlink: false,
        root_dev: RootDev(ident(path.parent().unwrap()).dev),
        is_mount_point: false,
    };
    match decide_path_common(&facts, &roots_for(f)) {
        GuardDecision::Allow(a) => a,
        GuardDecision::Skip(r) => panic!("guard refused {}: {r:?}", path.display()),
    }
}

fn dir_identity(id: Identity) -> ItemIdentity {
    ItemIdentity {
        dev: id.dev,
        ino: id.ino,
        kind: FileKind::Dir,
    }
}

fn run(
    f: &Fixture,
    witness: &Allowed,
    planned: Identity,
    expect: Identity,
    target: &Path,
) -> Result<(), DeleteError> {
    let ops = RealDirOps::confined_to(&f.code);
    let root = open_verified_root(&ops, &f.code, ident(&f.code)).unwrap();
    let opened = ops.fstat(root.dir()).unwrap();
    let policy = DeletePolicy::for_allowed_roots(
        witness,
        RootDev(opened.ident.dev),
        opened.mnt_id.unwrap_or(1),
        &roots_for(f),
        dir_identity(planned),
    )
    .unwrap();
    let rel = target.parent().unwrap().strip_prefix(&f.code).unwrap();
    let parent = open_parent(&ops, &root, rel).unwrap();
    delete_tree(
        &ops,
        &parent,
        target.file_name().unwrap(),
        expect,
        &policy,
        &AbortFlag::new(),
        &ApplyToken::for_tests(),
    )
    .map(|_| ())
    .map_err(|e| e.error)
}

fn marked(f: &Fixture) -> std::path::PathBuf {
    let dir = f.code.join("app/data");
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("sub/x"), b"x").unwrap();
    dir
}

#[test]
fn delete_tree_should_remove_the_directory_When_expect_planned_and_the_directory_agree() {
    let f = fixture();
    let dir = marked(&f);
    let id = ident(&dir);
    let w = witness_seen_as(&f, &dir, id);
    run(&f, &w, id, id, &dir).unwrap();
    assert!(!dir.exists());
}

#[test]
fn safety_delete_tree_should_refuse_and_keep_the_tree_When_expect_is_the_fresh_identity_but_the_plan_saw_another(
) {
    let f = fixture();
    let dir = marked(&f);
    let fresh = ident(&dir);
    let planned = Identity {
        dev: fresh.dev,
        ino: fresh.ino ^ 0x5a5a_5a5a,
    };
    // The plan saw `planned`; the cleaner template would pass the fresh identity as `expect`.
    let w = witness_seen_as(&f, &dir, planned);
    assert_eq!(
        run(&f, &w, planned, fresh, &dir),
        Err(DeleteError::IdentityMismatch)
    );
    assert!(dir.join("sub/x").exists(), "the tree is intact");
}

#[test]
fn for_allowed_roots_should_refuse_before_any_delete_When_the_planned_identity_is_not_the_witnesss()
{
    let f = fixture();
    let dir = marked(&f);
    let fresh = ident(&dir);
    let w = witness_seen_as(&f, &dir, fresh);
    let stale = Identity {
        dev: fresh.dev,
        ino: fresh.ino ^ 1,
    };
    let ops = RealDirOps::confined_to(&f.code);
    let root = open_verified_root(&ops, &f.code, ident(&f.code)).unwrap();
    let opened = ops.fstat(root.dir()).unwrap();
    let r = DeletePolicy::for_allowed_roots(
        &w,
        RootDev(opened.ident.dev),
        opened.mnt_id.unwrap_or(1),
        &roots_for(&f),
        dir_identity(stale),
    );
    assert!(r.is_err());
    assert!(dir.join("sub/x").exists());
}

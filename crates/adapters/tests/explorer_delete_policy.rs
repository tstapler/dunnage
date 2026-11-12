#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Review C1 to C3 of Epic 6.6 (ADR-006): an explorer delete reaches `delete_tree` only through the
//! `Allowed` witness of `decide_path_common`, a `.git` path never gets one, and a policy minted for one path
//! refuses any other. Real tempdir fixtures; nothing outside them is touched.

mod common;

use std::fs;
use std::path::Path;

use common::discovery::{canon, fixture, roots_for, Fixture};
use dunnage_adapters::{delete_tree, open_parent, open_verified_root, RealDirOps};
use dunnage_core::guard::{decide_path_common, Facts, GuardDecision};
use dunnage_core::{
    AbortFlag, Allowed, AllowedTarget, ApplyToken, DeleteError, DeletePolicy, Identity, Kind,
    RootDev, SkipReason,
};

fn ident(p: &Path) -> Identity {
    Identity::of(&fs::symlink_metadata(p).unwrap())
}

fn facts(path: &Path) -> Facts {
    let root_dev = RootDev(ident(path.parent().unwrap()).dev);
    Facts {
        kind: Kind::ExplorerSelection,
        path: path.to_path_buf(),
        ident: ident(path),
        is_symlink: false,
        root_dev,
        is_mount_point: false,
    }
}

fn scope_for(f: &Fixture, path: &Path) -> Result<Allowed, SkipReason> {
    match decide_path_common(&facts(path), &roots_for(f)) {
        GuardDecision::Allow(a) => Ok(a),
        GuardDecision::Skip(r) => Err(r),
    }
}

fn policy(f: &Fixture, scope: &Allowed) -> DeletePolicy {
    let roots = roots_for(f);
    let mnt = RealDirOps::confined_to(&f.code);
    let root = open_verified_root(&mnt, &f.code, ident(&f.code)).unwrap();
    let opened = dunnage_adapters::DirOps::fstat(&mnt, root.dir()).unwrap();
    DeletePolicy::for_allowed(
        scope,
        RootDev(ident(&f.code).dev),
        opened.mnt_id.unwrap_or(1),
        roots.home().to_path_buf(),
        canon(roots.home()).unwrap().1,
        roots.scan_roots().to_vec(),
    )
    .unwrap()
}

fn run(f: &Fixture, p: &DeletePolicy, target: &Path) -> Result<(), DeleteError> {
    let ops = RealDirOps::confined_to(&f.code);
    let root = open_verified_root(&ops, &f.code, ident(&f.code)).unwrap();
    let rel = target.parent().unwrap().strip_prefix(&f.code).unwrap();
    let parent = open_parent(&ops, &root, rel).unwrap();
    delete_tree(
        &ops,
        &parent,
        target.file_name().unwrap(),
        ident(target),
        p,
        &AbortFlag::new(),
        &ApplyToken::for_tests(),
    )
    .map(|_| ())
    .map_err(|e| e.error)
}

#[test]
fn delete_tree_should_remove_a_marked_directory_When_the_policy_is_built_from_its_allowed_witness()
{
    let f = fixture();
    let dir = f.code.join("app/data");
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("sub/x"), b"x").unwrap();
    let scope = scope_for(&f, &dir).expect("a directory below the root is allowed");
    run(&f, &policy(&f, &scope), &dir).unwrap();
    assert!(!dir.exists());
    assert!(f.code.join("app").exists(), "the parent stays");
}

#[test]
fn decide_path_common_should_mint_no_witness_for_a_dot_git_directory_so_no_explorer_policy_exists()
{
    let f = fixture();
    let git = f.code.join("app/.git");
    fs::create_dir_all(git.join("objects")).unwrap();
    assert_eq!(scope_for(&f, &git).unwrap_err(), SkipReason::ProtectedPath);
    assert!(git.exists());
}

#[test]
fn safety_explorer_scope_made_for_one_path_refuses_dot_git_and_leaves_it_When_reused() {
    // The review's PoC: an Exact policy deleted `home/code/app/.git`. A scope minted for another path
    // validates only that path.
    let f = fixture();
    let ok = f.code.join("app/data");
    let git = f.code.join("app/.git");
    fs::create_dir_all(&ok).unwrap();
    fs::create_dir_all(git.join("objects")).unwrap();
    let scope = scope_for(&f, &ok).unwrap();
    let mut p = policy(&f, &scope);
    p.target = git.clone();
    assert_eq!(run(&f, &p, &git), Err(DeleteError::OutsideScope));
    assert!(git.join("objects").exists());
}

#[test]
fn for_allowed_should_build_only_a_BelowScanRoot_policy_with_the_roots_mount_id_and_no_marker() {
    let f = fixture();
    let ok = f.code.join("app/data");
    fs::create_dir_all(&ok).unwrap();
    let p = policy(&f, &scope_for(&f, &ok).unwrap());
    assert!(matches!(p.allowed, AllowedTarget::BelowScanRoot(_)));
    assert!(p.root_mnt.is_some());
    assert!(p.require_sibling_any.is_empty());
}

#[test]
fn delete_tree_should_refuse_a_regular_file_with_the_engines_own_error_When_one_is_handed_to_it() {
    // Why the explorer marks directories only: the engine cannot delete a file (review C3).
    let f = fixture();
    let file = f.code.join("app/notes.txt");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, b"keep").unwrap();
    let scope = scope_for(&f, &file).unwrap();
    assert_eq!(
        run(&f, &policy(&f, &scope), &file),
        Err(DeleteError::Symlink)
    );
    assert!(file.exists());
}

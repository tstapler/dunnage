#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Regression: an inode with three names inside the tree is counted once (names after the first see a
//! smaller `st_nlink` because earlier names are already unlinked). Tempdirs only.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use disk_cleanup_adapters::{delete_tree, open_parent, open_verified_root, RealDirOps};
use disk_cleanup_core::{
    AbortFlag, AllowedTarget, ApplyToken, DeletePolicy, Identity, NamePattern, RootDev,
};

#[test]
fn Freed_should_count_an_inode_once_When_it_has_three_names_inside_the_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let nm = home.join("code/app/node_modules");
    fs::create_dir_all(&nm).unwrap();
    fs::write(home.join("code/app/package.json"), "{}").unwrap();
    fs::write(nm.join("a"), vec![1u8; 65_536]).unwrap();
    fs::hard_link(nm.join("a"), nm.join("b")).unwrap();
    fs::hard_link(nm.join("a"), nm.join("c")).unwrap();
    let file_bytes = fs::metadata(nm.join("a")).unwrap().blocks() * 512;
    let dir_bytes = fs::metadata(&nm).unwrap().blocks() * 512;
    let id = |p: &Path| Identity::of(&fs::symlink_metadata(p).unwrap());
    let policy = DeletePolicy {
        root_dev: RootDev(fs::metadata(&base).unwrap().dev()),
        root_mnt: None,
        home: home.clone(),
        home_id: id(&home),
        scan_roots: vec![home.join("code")],
        allowed: AllowedTarget::Within {
            root: home.join("code"),
            name: NamePattern::NodeModules,
        },
        target: nm.clone(),
        require_sibling_any: vec![OsString::from("package.json")],
        max_depth: 64,
        restore_owner_write: false,
    };
    let ops = RealDirOps::confined_to(&base);
    let root = open_verified_root(&ops, &base, id(&base)).unwrap();
    let parent = open_parent(&ops, &root, Path::new("home/code/app")).unwrap();
    let f = delete_tree(
        &ops,
        &parent,
        "node_modules".as_ref(),
        id(&nm),
        &policy,
        &AbortFlag::new(),
        &ApplyToken::for_tests(),
    )
    .unwrap();
    assert_eq!(f.attributed_bytes.0, file_bytes + dir_bytes);
    assert_eq!(f.inodes_freed, 2);
    assert_eq!(f.entries_removed, 4);
}

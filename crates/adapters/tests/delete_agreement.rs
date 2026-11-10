#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! `treestat::measure` and `delete_tree` count by one rule (Glossary "Counting rule", Story 2.1.2). Tempdirs only.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};

use disk_cleanup_adapters::treestat::{measure, MeasureOpts};
use disk_cleanup_adapters::{delete_tree, open_parent, open_verified_root, RealDirOps};
use disk_cleanup_core::{
    AbortFlag, AllowedTarget, ApplyToken, DeletePolicy, Freed, Identity, NamePattern, RootDev,
};

struct World {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    nm: PathBuf,
}

fn ident(p: &Path) -> Identity {
    Identity::of(&fs::symlink_metadata(p).unwrap())
}

fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let nm = base.join("home/code/app/node_modules");
    fs::create_dir_all(&nm).unwrap();
    fs::write(base.join("home/code/app/package.json"), "{}").unwrap();
    World {
        _tmp: tmp,
        base,
        nm,
    }
}

fn fill(nm: &Path) {
    fs::create_dir_all(nm.join("a/b/c")).unwrap();
    fs::create_dir_all(nm.join("empty")).unwrap();
    fs::write(nm.join("top"), vec![1u8; 5000]).unwrap();
    fs::write(nm.join("a/f"), vec![2u8; 70_000]).unwrap();
    fs::write(nm.join("a/b/c/g"), b"x").unwrap();
    fs::hard_link(nm.join("a/f"), nm.join("a/b/f_link")).unwrap();
    symlink("/nonexistent-target", nm.join("a/dangling")).unwrap();
}

fn policy(w: &World) -> DeletePolicy {
    let home = w.base.join("home");
    DeletePolicy {
        root_dev: RootDev(fs::metadata(&w.base).unwrap().dev()),
        root_mnt: None,
        home: home.clone(),
        home_id: ident(&home),
        scan_roots: vec![home.join("code")],
        allowed: AllowedTarget::Within {
            root: home.join("code"),
            name: NamePattern::NodeModules,
        },
        target: w.nm.clone(),
        require_sibling_any: vec![OsString::from("package.json")],
        max_depth: 64,
        restore_owner_write: false,
    }
}

fn delete(w: &World) -> Freed {
    let ops = RealDirOps::confined_to(&w.base);
    let root = open_verified_root(&ops, &w.base, ident(&w.base)).unwrap();
    let parent = open_parent(&ops, &root, Path::new("home/code/app")).unwrap();
    delete_tree(
        &ops,
        &parent,
        "node_modules".as_ref(),
        ident(&w.nm),
        &policy(w),
        &AbortFlag::new(),
        &ApplyToken::for_tests(),
    )
    .unwrap()
}

fn measured(w: &World) -> disk_cleanup_adapters::treestat::TreeStats {
    let ops = RealDirOps::confined_to(&w.base);
    let root = open_verified_root(&ops, &w.base, ident(&w.base)).unwrap();
    let parent = open_parent(&ops, &root, Path::new("home/code/app")).unwrap();
    let dir = {
        use disk_cleanup_adapters::DirOps;
        ops.open_dir(parent.dir(), "node_modules".as_ref()).unwrap()
    };
    measure(&ops, &dir, policy(w).root_dev, MeasureOpts::default())
}

#[test]
fn treestat_measure_should_equal_delete_tree_Freed_When_no_link_leaves_the_tree() {
    let w = world();
    fill(&w.nm);
    let m = measured(&w);
    let f = delete(&w);
    assert!(!w.nm.exists());
    assert_eq!(m.allocated, f.attributed_bytes);
    assert_eq!(m.apparent, f.apparent_bytes);
    assert_eq!(m.inodes, f.inodes_freed);
    assert_eq!(m.entries, f.entries_removed);
    assert!(f.entries_removed >= 9, "{f:?}");
}

#[test]
fn treestat_allocated_should_exceed_Freed_When_a_hardlink_leaves_the_tree() {
    let w = world();
    fill(&w.nm);
    let outside = w.base.join("outside.bin");
    fs::write(&outside, vec![3u8; 65_536]).unwrap();
    fs::hard_link(&outside, w.nm.join("a/linked")).unwrap();
    let blocks = fs::metadata(&outside).unwrap().blocks() * 512;
    let m = measured(&w);
    let f = delete(&w);
    assert_eq!(
        m.allocated.0,
        f.attributed_bytes.0 + blocks,
        "upper bound by the outside inode"
    );
    assert_eq!(m.inodes, f.inodes_freed + 1);
    assert_eq!(m.entries, f.entries_removed, "names are counted either way");
    assert_eq!(m.apparent, f.apparent_bytes);
}

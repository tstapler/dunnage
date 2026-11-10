#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Differential and property tests for `delete_tree` (Story 2.1.3, A-30 and A-31). Tempdirs only.
//! The `std::fs::remove_dir_all` call lives here, outside `crates/*/src`, so `check-no-stray-unlink.sh` does not see it.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use disk_cleanup_adapters::{delete_tree, open_parent, open_verified_root, RealDirOps};
use disk_cleanup_core::{
    AbortFlag, AllowedTarget, ApplyToken, DeletePolicy, Freed, Identity, NamePattern, RootDev,
};
use proptest::prelude::*;

/// One generated entry: a chain of `path` directories (each index < 6, at most 11 long), then one leaf.
#[derive(Debug, Clone)]
struct Entry {
    path: Vec<u8>,
    kind: u8,
    size: usize,
}

fn entries() -> impl Strategy<Value = Vec<Entry>> {
    prop::collection::vec(
        (prop::collection::vec(0u8..6, 0..=11), 0u8..5, 0usize..9000)
            .prop_map(|(path, kind, size)| Entry { path, kind, size }),
        0..40,
    )
}

struct World {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    nm: PathBuf,
    sentinel: PathBuf,
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
    let sentinel = base.join("sentinel");
    fs::create_dir_all(sentinel.join("sub")).unwrap();
    fs::write(sentinel.join("keep.txt"), b"keep").unwrap();
    fs::write(sentinel.join("sub/deep.txt"), vec![7u8; 4097]).unwrap();
    World {
        _tmp: tmp,
        base,
        nm,
        sentinel,
    }
}

/// Builds the generated tree under `nm`. Symlinks point at the sentinel, which must never be followed.
/// Hardlinks only join files inside the tree. Read-only directories are empty so `remove_dir_all` can remove them too.
fn build(w: &World, spec: &[Entry], hardlinks: bool) {
    let mut files: Vec<PathBuf> = Vec::new();
    for (i, e) in spec.iter().enumerate() {
        let mut dir = w.nm.clone();
        for c in &e.path {
            dir.push(format!("d{c}"));
        }
        fs::create_dir_all(&dir).unwrap();
        let leaf = dir.join(format!("n{i}"));
        match e.kind {
            0 => {
                fs::write(&leaf, vec![1u8; e.size]).unwrap();
                files.push(leaf);
            }
            1 => fs::create_dir(&leaf).unwrap(),
            2 => symlink(&w.sentinel, &leaf).unwrap(),
            3 => {
                fs::create_dir(&leaf).unwrap();
                fs::set_permissions(&leaf, fs::Permissions::from_mode(0o555)).unwrap();
            }
            _ => {
                if hardlinks && !files.is_empty() {
                    fs::hard_link(&files[e.size % files.len()], &leaf).unwrap();
                } else {
                    symlink(w.sentinel.join("keep.txt"), &leaf).unwrap();
                }
            }
        }
    }
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

fn hash_tree(root: &Path) -> u64 {
    fn walk(p: &Path, h: &mut DefaultHasher, root: &Path) {
        let mut names: Vec<_> = fs::read_dir(p)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        names.sort();
        for n in names {
            let m = fs::symlink_metadata(&n).unwrap();
            n.strip_prefix(root).unwrap().hash(h);
            m.mode().hash(h);
            m.len().hash(h);
            m.nlink().hash(h);
            if m.is_dir() {
                walk(&n, h, root);
            } else if m.is_file() {
                fs::read(&n).unwrap().hash(h);
            }
        }
    }
    let mut h = DefaultHasher::new();
    walk(root, &mut h, root);
    h.finish()
}

fn listing(dir: &Path) -> Vec<OsString> {
    let mut v: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    v.sort();
    v
}

/// Sum of `blocks * 512` over distinct `(dev, ino)` under `root`, root included, never following symlinks.
fn independent_count(root: &Path) -> u64 {
    fn walk(p: &Path, seen: &mut HashSet<(u64, u64)>, total: &mut u64) {
        let m = fs::symlink_metadata(p).unwrap();
        if seen.insert((m.dev(), m.ino())) {
            *total += m.blocks() * 512;
        }
        if m.is_dir() {
            for e in fs::read_dir(p).unwrap() {
                walk(&e.unwrap().path(), seen, total);
            }
        }
    }
    let mut total = 0;
    walk(root, &mut HashSet::new(), &mut total);
    total
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 200, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn delete_tree_should_leave_same_remaining_entries_as_remove_dir_all_and_never_change_sentinel_When_200_random_trees(spec in entries()) {
        let a = world();
        let b = world();
        build(&a, &spec, false);
        build(&b, &spec, false);
        let sentinel_before = hash_tree(&a.sentinel);

        delete(&a);
        fs::remove_dir_all(&b.nm).unwrap();

        prop_assert!(fs::symlink_metadata(&a.nm).is_err());
        prop_assert!(fs::symlink_metadata(&b.nm).is_err());
        prop_assert_eq!(listing(a.nm.parent().unwrap()), listing(b.nm.parent().unwrap()));
        prop_assert_eq!(hash_tree(&a.sentinel), sentinel_before);
        prop_assert_eq!(hash_tree(&a.sentinel), hash_tree(&b.sentinel));
    }

    #[test]
    fn freed_attributed_bytes_should_equal_sum_blocks_times_512_over_distinct_dev_ino_When_random_hardlinks(spec in entries()) {
        let w = world();
        build(&w, &spec, true);
        let expected = independent_count(&w.nm);
        let freed = delete(&w);
        prop_assert!(fs::symlink_metadata(&w.nm).is_err());
        prop_assert_eq!(freed.attributed_bytes.0, expected);
    }
}

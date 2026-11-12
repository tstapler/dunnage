#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! treestat against real tempdirs only; every ops value is confined to the tempdir.

use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use dunnage_adapters::testing::{At, FaultDirOps, Op};
use dunnage_adapters::treestat::{measure, MeasureOpts, SkipWhy};
use dunnage_adapters::{open_verified_root, RealDirOps};
use dunnage_core::{AllocatedBytes, ApparentBytes, Identity, RootDev};

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().canonicalize().unwrap();
    (t, p)
}

fn dev_of(p: &Path) -> u64 {
    std::fs::metadata(p).unwrap().dev()
}

fn write_4k(p: &Path) {
    std::fs::write(p, vec![1u8; 4096]).unwrap();
}

#[test]
fn treestat_should_count_hardlinks_once_and_allocated_as_blocks_times_512_When_three_files_two_linked(
) {
    let (_t, p) = tmp();
    write_4k(&p.join("a"));
    std::fs::hard_link(p.join("a"), p.join("b")).unwrap();
    write_4k(&p.join("c"));
    let ops = RealDirOps::confined_to(&p);
    let root = open_verified_root(&ops, &p, Identity::of(&std::fs::metadata(&p).unwrap())).unwrap();
    let st = measure(
        &ops,
        root.dir(),
        RootDev(dev_of(&p)),
        MeasureOpts::default(),
    );
    let blocks = std::fs::metadata(p.join("a")).unwrap().blocks() * 512;
    let dir_blocks = std::fs::metadata(&p).unwrap().blocks() * 512;
    assert_eq!(st.allocated, AllocatedBytes(2 * blocks + dir_blocks));
    assert_eq!(
        st.apparent,
        ApparentBytes(3 * 4096),
        "per name, directories add 0"
    );
    assert_eq!(st.inodes, 3, "two file inodes and the measured directory");
    assert_eq!(st.entries, 4, "three names and the measured directory");
    assert!(st.newest_mtime.is_some());
    assert!(st.skipped.is_empty());
    assert!(!st.stopped_early);
}

#[test]
fn treestat_should_recurse_and_skip_other_device_subtrees_and_symlinks_When_bounded_to_root_dev() {
    let (_t, p) = tmp();
    std::fs::create_dir_all(p.join("same/deep")).unwrap();
    std::fs::create_dir_all(p.join("other")).unwrap();
    write_4k(&p.join("same/deep/f"));
    write_4k(&p.join("other/big"));
    let outside = tempfile::tempdir().unwrap();
    write_4k(&outside.path().join("o"));
    symlink(outside.path(), p.join("link")).unwrap();
    let real = RealDirOps::confined_to(&p);
    let ops = FaultDirOps::wrap(real)
        .report_dev(At::Any, 41)
        .report_dev(At::Name("other".into()), 42)
        .build();
    let root = open_verified_root(
        &ops,
        &p,
        Identity {
            dev: 41,
            ino: std::fs::metadata(&p).unwrap().ino(),
        },
    );
    // The root's own fstat reports dev 41 as well, so identity must be built with it.
    let root = root.unwrap();
    let st = measure(&ops, root.dir(), RootDev(41), MeasureOpts::default());
    let link_len = std::fs::symlink_metadata(p.join("link")).unwrap().len();
    assert_eq!(
        st.apparent,
        ApparentBytes(4096 + link_len),
        "same/deep/f and the link itself count; the link's target and the other device do not"
    );
    let mut why: Vec<_> = st.skipped.iter().map(|s| (s.rel.clone(), s.why)).collect();
    why.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        why,
        vec![
            (PathBuf::from("link"), SkipWhy::Symlink),
            (PathBuf::from("other"), SkipWhy::OtherDevice)
        ]
    );
}

#[test]
fn treestat_should_stop_without_visiting_the_rest_When_an_entry_is_newer_than_the_bound() {
    let (_t, p) = tmp();
    for i in 0..50 {
        write_4k(&p.join(format!("f{i:02}")));
    }
    let ops = FaultDirOps::wrap(RealDirOps::confined_to(&p)).build();
    let root = open_verified_root(&ops, &p, Identity::of(&std::fs::metadata(&p).unwrap())).unwrap();
    let bound = SystemTime::now() - Duration::from_secs(3600);
    let st = measure(
        &ops,
        root.dir(),
        RootDev(dev_of(&p)),
        MeasureOpts {
            stop_if_newer_than: Some(bound),
        },
    );
    assert!(st.stopped_early);
    assert!(st.newest_mtime.unwrap() >= bound);
    assert_eq!(
        ops.calls(Op::StatAt),
        1,
        "first entry is newer, nothing else is visited"
    );

    let none = measure(
        &ops,
        root.dir(),
        RootDev(dev_of(&p)),
        MeasureOpts::default(),
    );
    assert!(!none.stopped_early);
    assert_eq!(none.entries, 51, "50 files and the measured directory");
}

#[test]
fn treestat_should_record_unreadable_entries_in_skipped_When_stat_fails() {
    let (_t, p) = tmp();
    write_4k(&p.join("ok"));
    write_4k(&p.join("bad"));
    let ops = FaultDirOps::wrap(RealDirOps::confined_to(&p))
        .errno_on(Op::StatAt, At::Name("bad".into()), libc::EACCES)
        .build();
    let root = open_verified_root(&ops, &p, Identity::of(&std::fs::metadata(&p).unwrap())).unwrap();
    let st = measure(
        &ops,
        root.dir(),
        RootDev(dev_of(&p)),
        MeasureOpts::default(),
    );
    assert_eq!(st.entries, 2, "ok and the measured directory");
    assert_eq!(st.skipped.len(), 1);
    assert_eq!(st.skipped[0].why, SkipWhy::Unreadable(libc::EACCES));
}

fn vm_hwm_kib() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    let l = s.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
    l.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// Local gate test: needs `DC_TEST_ROOT` (a scratch directory under `$HOME/.cache/dc-fixtures`), builds 500,000
/// files, then 500,000 hardlinks. Hardlinks are spread over 10 inodes because filesystems cap the link
/// count of one inode (ext4: 65,000), which makes the plan's "500,000 links to one inode" unbuildable there.
#[test]
#[ignore = "local gate: 500,000 files under DC_TEST_ROOT"]
fn treestat_should_use_memory_independent_of_entry_count_When_measuring_500_000_files() {
    let root = PathBuf::from(std::env::var("DC_TEST_ROOT").expect("DC_TEST_ROOT"));
    let root = root.canonicalize().unwrap();
    let work = tempfile::tempdir_in(&root).unwrap();
    let base = work.path().canonicalize().unwrap();
    assert!(base.starts_with(&root));

    let flat = base.join("flat");
    std::fs::create_dir(&flat).unwrap();
    for i in 0..500_000u32 {
        std::fs::File::create(flat.join(format!("f{i}"))).unwrap();
    }
    let links = base.join("links");
    std::fs::create_dir(&links).unwrap();
    for g in 0..10u32 {
        let origin = links.join(format!("origin{g}"));
        std::fs::write(&origin, b"x").unwrap();
        for i in 0..49_999u32 {
            std::fs::hard_link(&origin, links.join(format!("l{g}_{i}"))).unwrap();
        }
    }

    for (dir, inodes) in [(&flat, 500_001u64), (&links, 11)] {
        let ops = RealDirOps::confined_to(&base);
        let r =
            open_verified_root(&ops, dir, Identity::of(&std::fs::metadata(dir).unwrap())).unwrap();
        let before = vm_hwm_kib();
        let st = measure(&ops, r.dir(), RootDev(dev_of(dir)), MeasureOpts::default());
        let grown = vm_hwm_kib().saturating_sub(before);
        assert_eq!(st.inodes, inodes);
        assert!(
            grown < 64 * 1024,
            "VmHWM grew {grown} KiB over {}",
            dir.display()
        );
    }
}

#[test]
fn measure_should_count_every_dot_git_entry_below_the_directory() {
    use dunnage_adapters::treestat::{measure, MeasureOpts};
    use dunnage_adapters::{DirOps, RealDirOps};
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    for d in ["a/.git/objects", "b/c/.git", "plain"] {
        std::fs::create_dir_all(base.join(d)).unwrap();
    }
    std::fs::create_dir_all(base.join("wt")).unwrap();
    std::fs::write(base.join("wt/.git"), b"gitdir: x").unwrap();
    let ops = RealDirOps::confined_to(&base);
    let dir = ops.open_root(&base).unwrap();
    let dev = dunnage_core::RootDev(ops.fstat(&dir).unwrap().ident.dev);
    let stats = measure(&ops, &dir, dev, MeasureOpts::default());
    assert_eq!(
        stats.git_repos, 3,
        "two directories and one worktree link file"
    );
}

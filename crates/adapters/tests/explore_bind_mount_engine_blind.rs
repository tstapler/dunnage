#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.6.1, rule M3: why the mark check refuses a directory with a same-device bind mount below it.
//! The engine's only signal for "another filesystem" is `st_dev` (`RootDev`, `Identity::on_dev`). A bind mount
//! of the same device shows the parent's `st_dev`, so the engine's view of it equals an ordinary
//! subdirectory's; only the mount table (which `mark_check` reads) tells them apart. Nothing is deleted:
//! this reads `fstat` and `fstatat` of a tempdir through the engine's `DirOps` port.

use std::ffi::OsStr;

use disk_cleanup_adapters::testing::{At, FaultDirOps};
use disk_cleanup_adapters::{DirOps, RealDirOps};
use disk_cleanup_core::RootDev;

#[test]
fn a_same_device_bind_mount_looks_to_the_engine_like_an_ordinary_subdirectory() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    std::fs::create_dir(base.join("plain")).unwrap();
    std::fs::create_dir(base.join("bind")).unwrap(); // stands for a bind mount: same st_dev as its parent
    std::fs::create_dir(base.join("mnt")).unwrap(); // stands for a mount of another device
    let ops = FaultDirOps::wrap(RealDirOps::confined_to(&base))
        .report_dev(At::Name("mnt".into()), 4242)
        .build();
    let root = ops.open_root(&base).unwrap();
    let parent_dev = RootDev(ops.fstat(&root).unwrap().ident.dev);
    let on_parent_dev = |name: &str| {
        ops.stat_at(&root, OsStr::new(name))
            .unwrap()
            .ident
            .on_dev(parent_dev)
    };
    assert!(on_parent_dev("plain"));
    assert!(
        on_parent_dev("bind"),
        "the engine's st_dev test cannot tell a bind mount from a subdirectory, so it would walk into it"
    );
    assert!(
        !on_parent_dev("mnt"),
        "a mount of another device is the one case the st_dev test does catch"
    );
}

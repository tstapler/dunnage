#![allow(clippy::unwrap_used, non_snake_case)]
//! Task 6.6.5j (ADR-006 A10): `RootSet::resolve_on` refuses a root on a pseudo filesystem.

use std::io;
use std::path::{Path, PathBuf};

use disk_cleanup_core::roots::{RootError, RootSet};
use disk_cleanup_core::{Dirs, Identity};

fn dirs() -> Dirs {
    Dirs {
        home: "/home/u".into(),
        state: "/home/u/.local/state".into(),
        cache: "/home/u/.cache".into(),
        config: "/home/u/.config".into(),
    }
}

#[allow(clippy::unnecessary_wraps)]
fn canon(p: &Path) -> io::Result<(PathBuf, Identity)> {
    Ok((
        p.to_path_buf(),
        Identity {
            dev: 1,
            ino: p.as_os_str().len() as u64,
        },
    ))
}

fn resolve(roots: &[&str], magic: u64) -> Result<RootSet, RootError> {
    let list: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
    RootSet::resolve_on(&dirs(), &[], Some(&list), Path::new("/"), &canon, &|p| {
        p.starts_with("/sys").then_some(magic)
    })
}

#[test]
fn safety_explorer_roots_on_pseudo_filesystems_are_refused() {
    for (magic, name) in [
        (0x6265_6572_u64, "sysfs"),
        (0x9fa0, "proc"),
        (0x0027_e0eb, "cgroup"),
        (0x6367_7270, "cgroup2"),
        (0x7363_6673, "securityfs"),
    ] {
        let err = resolve(&["/data/ok", "/sys/fs/cgroup"], magic).unwrap_err();
        let text = err.text();
        assert!(text.contains(name), "{text}");
        assert!(
            text.contains("/sys/fs/cgroup") && text.contains("/data/ok"),
            "{text}"
        );
        assert_eq!(err.exit_code(), 64);
    }
}

#[test]
fn resolve_on_should_refuse_a_tmpfs_root_at_or_below_dev_as_devtmpfs_and_not_one_elsewhere() {
    let list = vec![PathBuf::from("/dev/shm")];
    let tmpfs = |_: &Path| Some(0x0102_1994_u64);
    let err =
        RootSet::resolve_on(&dirs(), &[], Some(&list), Path::new("/"), &canon, &tmpfs).unwrap_err();
    assert!(err.text().contains("devtmpfs"), "{}", err.text());
    let elsewhere = vec![PathBuf::from("/tmp/x")];
    RootSet::resolve_on(
        &dirs(),
        &[],
        Some(&elsewhere),
        Path::new("/"),
        &canon,
        &tmpfs,
    )
    .unwrap();
}

#[test]
fn resolve_on_should_accept_ordinary_filesystems() {
    for magic in [0xef53_u64, 0x9123_683e, 0x5846_5342, 0x0102_1994] {
        let rs = resolve(&["/sys/x", "/data/ok"], magic).unwrap();
        assert_eq!(rs.scan_roots().len(), 2);
    }
}

#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.1.2: boundaries, unreadable and vanished directories are events, not silence.

use disk_cleanup_scan::index::{Backend, CollectRequest, DirEvent, StreamRequest};
use disk_cleanup_scan::walker::{CacheConfig, WalkerBackend};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const INNER: &str = "DC_SCAN_BOUNDARY_ROOT";

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dc-scan-boundary-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    fs::canonicalize(&d).unwrap()
}

fn walk(backend: &WalkerBackend, root: &Path) -> Vec<DirEvent> {
    let roots = vec![root.to_path_buf()];
    let req = StreamRequest::new(
        CollectRequest {
            roots: &roots,
            cross_fs: false,
            deadline: None,
            threads: 2,
        },
        Arc::new(AtomicBool::new(false)),
    );
    let got = Mutex::new(Vec::new());
    backend
        .collect_streaming(&req, &|e| got.lock().unwrap().extend(e))
        .unwrap();
    got.into_inner().unwrap()
}

/// Runs inside `unshare -rm` with a tmpfs mounted at `<root>/mnt` (see the outer test).
#[test]
fn inner_walk_should_report_a_boundary_and_list_nothing_below_it() {
    let Ok(root) = std::env::var(INNER) else {
        return;
    };
    let root = PathBuf::from(root);
    let events = walk(&WalkerBackend::default(), &root);
    let mnt_dev = fs::metadata(root.join("mnt")).unwrap().dev();
    let boundary = events
        .iter()
        .find_map(|e| match e {
            DirEvent::Boundary { name, dev, .. } => Some((name.clone(), *dev)),
            _ => None,
        })
        .expect("a Boundary event");
    assert_eq!(boundary, ("mnt".into(), mnt_dev));
    let names: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            DirEvent::Dir { name, .. } => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    assert!(
        !names.contains(&"below".to_string()),
        "nothing below the mount is listed: {names:?}"
    );
}

#[test]
fn boundary_event_should_appear_When_a_tmpfs_is_mounted_below_the_root_in_unshare() {
    if std::env::var(INNER).is_ok() {
        return;
    }
    let probe = Command::new("unshare").args(["-rm", "true"]).status();
    if !probe.is_ok_and(|s| s.success()) {
        println!("SKIP: unshare -rm is unavailable");
        return;
    }
    let root = tmp("mount");
    fs::create_dir_all(root.join("mnt")).unwrap();
    fs::create_dir_all(root.join("plain")).unwrap();
    let exe = std::env::current_exe().unwrap();
    let script = format!(
        "mount -t tmpfs tmpfs '{r}/mnt' && mkdir '{r}/mnt/below' && exec '{e}' --exact inner_walk_should_report_a_boundary_and_list_nothing_below_it --nocapture",
        r = root.display(),
        e = exe.display()
    );
    let out = Command::new("unshare")
        .args(["-rm", "sh", "-c", &script])
        .env(INNER, &root)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "inner run failed: {text}");
    assert!(text.contains("1 passed"), "inner test did not run: {text}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn unreadable_event_should_carry_errno_13_When_a_directory_has_mode_000() {
    if rustix::process::geteuid().is_root() {
        println!("SKIP: root reads mode-000 directories");
        return;
    }
    let root = tmp("eacces");
    fs::create_dir_all(root.join("locked")).unwrap();
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o0)).unwrap();
    let events = walk(&WalkerBackend::default(), &root);
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
    let _ = fs::remove_dir_all(&root);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DirEvent::Unreadable { name, errno: 13, .. } if name == "locked")),
        "{events:?}"
    );
}

#[test]
fn gone_event_should_be_emitted_When_a_cached_subdirectory_was_deleted_and_the_parent_mtime_restored(
) {
    use rustix::fs::{utimensat, AtFlags, Timespec, Timestamps, CWD};
    let root = tmp("gone");
    let cache_dir = tmp("gone-cache");
    fs::create_dir_all(root.join("a/b")).unwrap();
    let t = Timespec {
        tv_sec: 1_600_000_000,
        tv_nsec: 0,
    };
    let set = |p: &Path| {
        let ts = Timestamps {
            last_access: t,
            last_modification: t,
        };
        utimensat(CWD, p, &ts, AtFlags::empty()).unwrap();
    };
    for p in [root.join("a/b"), root.join("a"), root.clone()] {
        set(&p);
    }
    let backend = WalkerBackend {
        cache: Some(CacheConfig {
            path: cache_dir.join("c.bin"),
            rescan: false,
            max_age: Duration::from_secs(3600),
        }),
    };
    walk(&backend, &root);
    fs::remove_dir(root.join("a/b")).unwrap();
    set(&root.join("a"));
    let events = walk(&backend, &root);
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&cache_dir);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DirEvent::Gone { name, .. } if name == "b")),
        "{events:?}"
    );
    let mut ids: Vec<u64> = events.iter().map(DirEvent::id).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![0, 1, 2],
        "every id got exactly one terminal event"
    );
}

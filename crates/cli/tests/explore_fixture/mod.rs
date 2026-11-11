#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! A scripted scan of `/h/code` for the `--text` and `--json` tests of Story 6.5.5: no filesystem is read.

use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use disk_cleanup_cli::explore::text::ExploreEnv;
use disk_cleanup_core::explore::msg::{NodeEvent, ScanMode, ScanMsg, ScanSummary, NO_PARENT};
use disk_cleanup_core::explore::testing::{DirInfo, FakeScanSource, FakeTermSignals, Stats};
use disk_cleanup_core::ids::Identity;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::snapshot_types::{AllocSnapshot, FsSnapshot, SpaceUse};
use disk_cleanup_core::Dirs;

pub const GIB: u64 = 1 << 30;

pub fn stats(alloc: u64, inodes: u64) -> Stats {
    Stats {
        inodes,
        dirs: 1,
        files: inodes.saturating_sub(1),
        allocated_bytes: alloc,
        size_bytes: alloc + alloc / 8,
        ..Stats::default()
    }
}

fn dir(
    id: u64,
    parent: u64,
    name: OsString,
    alloc: u64,
    inodes: u64,
    pkg: bool,
    subdirs: u32,
) -> NodeEvent {
    NodeEvent::Dir {
        id,
        parent,
        name,
        dev: 1,
        info: DirInfo {
            stats: stats(alloc, inodes),
            mtime: 1_700_000_000 + id as i64,
            has_package_json: pkg,
            ..DirInfo::default()
        },
        subdirs,
        from_cache: false,
        painted: false,
    }
}

pub fn script(truncated: bool) -> Vec<ScanMsg> {
    let n = |s: &str| OsString::from(s);
    let events = vec![
        dir(0, NO_PARENT, n("/h/code"), 4096, 3, false, 6),
        dir(1, 0, n("app"), 33 * GIB, 494_800, true, 1),
        dir(2, 1, n("node_modules"), 21 * GIB, 400_000, false, 0),
        dir(3, 0, n("old-app"), 22 * GIB, 412_330, false, 0),
        dir(4, 0, n("caf\u{e9}"), 9 * GIB, 31_002, false, 0),
        dir(
            5,
            0,
            OsString::from_vec(b"bad\xffname".to_vec()),
            7 * GIB,
            9_120,
            false,
            0,
        ),
        NodeEvent::Unreadable {
            id: 6,
            parent: 0,
            name: n("root-owned"),
            errno: 13,
        },
        NodeEvent::Boundary {
            id: 7,
            parent: 0,
            name: n("mnt"),
            dev: 9,
        },
    ];
    vec![
        ScanMsg::Batch {
            generation: 0,
            mode: ScanMode::Fresh,
            events,
        },
        ScanMsg::Finished(ScanSummary {
            generation: 0,
            truncated,
            errors: 1,
            dirs_listed: 8,
            ..ScanSummary::default()
        }),
    ]
}

pub fn roots() -> RootSet {
    let dirs = Dirs {
        home: "/h".into(),
        state: "/h/.local/state".into(),
        cache: "/h/.cache".into(),
        config: "/h/.config".into(),
    };
    let canon = |p: &Path| -> io::Result<(PathBuf, Identity)> {
        let ino = p.as_os_str().len() as u64 * 31 + 7;
        Ok((p.to_path_buf(), Identity { dev: 1, ino }))
    };
    RootSet::resolve(
        &dirs,
        &[],
        Some(&[PathBuf::from("/h/code")]),
        Path::new("/h"),
        &canon,
    )
    .unwrap()
}

pub fn btrfs() -> FsSnapshot {
    FsSnapshot {
        path: "/".into(),
        taken_at: 0,
        total_bytes: 2_000 * GIB,
        used_bytes: 1_800 * GIB,
        avail_bytes: 70 * GIB,
        fs_type: "btrfs".into(),
        btrfs: Some(AllocSnapshot {
            data: SpaceUse {
                bytes_used: 1_700 * GIB,
                total_bytes: 1_750 * GIB,
            },
            metadata: SpaceUse {
                bytes_used: 14 * GIB,
                total_bytes: 16 * GIB,
            },
            system: SpaceUse {
                bytes_used: 1 << 20,
                total_bytes: 8 << 20,
            },
            unallocated: 9 * GIB,
        }),
    }
}

pub fn env(width: usize, truncated: bool) -> ExploreEnv {
    let mut e = ExploreEnv::new(
        Arc::new(FakeScanSource::new(script(truncated))),
        Arc::new(FakeTermSignals::never()),
        vec![PathBuf::from("/h/code")],
        roots(),
    );
    e.headroom = Ok(btrfs());
    e.width = width;
    e.elapsed_secs = Some(6.2);
    e.home = Some(PathBuf::from("/h"));
    e
}

pub fn golden(name: &str, got: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var_os("DC_UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, got).unwrap();
    }
    let want = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        got, want,
        "golden {name} differs (DC_UPDATE_GOLDEN=1 rewrites it)"
    );
}

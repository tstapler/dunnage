#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! The btrfs notes of a report (Story 4.3.1): the snapshot warning with at most three names, and the qgroup
//! warning read from a fixture sysfs.

use std::fs;
use std::path::{Path, PathBuf};

use disk_cleanup_adapters::snapshots::{SnapshotEntry, SnapshotInfo, SnapshotLayout, SnapshotSize};
use disk_cleanup_adapters::targets::btrfs_report::{
    qgroup_note, report_notes_from, snapshot_note, QGROUP_NOTE,
};
use disk_cleanup_core::ports::Env;
use disk_cleanup_core::testing::{
    FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs, FakePlanStore, FakeProbe, FakeRunGuard,
    FakeSpaceMeter,
};
use disk_cleanup_core::{Dirs, Platform};

fn entry(name: &str, date: Option<&str>) -> SnapshotEntry {
    SnapshotEntry {
        name: name.into(),
        date: date.map(Into::into),
        description: None,
        size: SnapshotSize::Unknown,
        root: PathBuf::from("/.snapshots").join(name).join("snapshot"),
    }
}

fn snapper(entries: Vec<SnapshotEntry>, readable: bool) -> SnapshotInfo {
    SnapshotInfo {
        layout: SnapshotLayout::Snapper,
        snapshots: entries,
        listing_readable: readable,
    }
}

#[test]
fn snapshot_note_should_name_the_snapshots_and_warn_about_overcounted_estimates() {
    let info = snapper(
        vec![
            entry("12", Some("2026-09-01 09:30:00")),
            entry("13", Some("2026-10-01 10:00:00")),
        ],
        true,
    );
    assert_eq!(
        snapshot_note(Some(&info)).unwrap(),
        "snapshots present (snapper: #12 2026-09-01, #13 2026-10-01): freed-bytes estimates may overcount shared extents"
    );
}

#[test]
fn snapshot_note_should_show_the_first_three_names_and_count_the_rest() {
    let many = |n: usize| {
        snapper(
            (1..=n)
                .map(|i| entry(&i.to_string(), Some("2026-01-02 00:00:00")))
                .collect(),
            true,
        )
    };
    assert_eq!(
        snapshot_note(Some(&many(3))).unwrap(),
        "snapshots present (snapper: #1 2026-01-02, #2 2026-01-02, #3 2026-01-02): freed-bytes estimates may overcount shared extents"
    );
    assert_eq!(
        snapshot_note(Some(&many(5))).unwrap(),
        "snapshots present (snapper: #1 2026-01-02, #2 2026-01-02, #3 2026-01-02, +2 more): freed-bytes estimates may overcount shared extents"
    );
}

#[test]
fn snapshot_note_should_use_the_directory_names_of_timeshift_and_skip_a_missing_date() {
    let t = SnapshotInfo {
        layout: SnapshotLayout::Timeshift,
        snapshots: vec![
            entry("2026-10-01_10-00-01", None),
            entry("2026-09-01_10-00-01", None),
        ],
        listing_readable: true,
    };
    assert_eq!(
        snapshot_note(Some(&t)).unwrap(),
        "snapshots present (timeshift: 2026-10-01_10-00-01, 2026-09-01_10-00-01): freed-bytes estimates may overcount shared extents"
    );
    let no_date = snapper(vec![entry("7", None)], true);
    assert!(snapshot_note(Some(&no_date))
        .unwrap()
        .contains("(snapper: #7)"));
}

#[test]
fn snapshot_note_should_be_absent_when_none_are_detected() {
    assert_eq!(snapshot_note(None), None);
    assert_eq!(snapshot_note(Some(&snapper(vec![], true))), None);
}

#[test]
fn snapshot_note_should_still_warn_when_the_snapshot_directory_is_not_listable() {
    let hidden = snapper(vec![], false);
    assert_eq!(
        snapshot_note(Some(&hidden)).unwrap(),
        "snapshots present (snapper: listing needs root): freed-bytes estimates may overcount shared extents"
    );
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    mountinfo: PathBuf,
    sysfs: PathBuf,
}

fn sysfs(fstype: &str, qgroups: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("mnt");
    fs::create_dir_all(&root).unwrap();
    let fs_dir = base.join("sysfs/uuid-1");
    fs::create_dir_all(fs_dir.join("devices/sda2")).unwrap();
    fs::create_dir_all(fs_dir.join("allocation")).unwrap();
    if qgroups {
        fs::create_dir_all(fs_dir.join("qgroups/0_5")).unwrap();
    }
    let mountinfo = base.join("mountinfo");
    fs::write(
        &mountinfo,
        format!(
            "36 1 0:26 / {} rw - {fstype} /dev/sda2 rw,subvolid=256\n",
            root.display()
        ),
    )
    .unwrap();
    Fixture {
        root,
        mountinfo,
        sysfs: base.join("sysfs"),
        _dir: dir,
    }
}

#[test]
fn qgroup_note_should_warn_that_balance_may_be_slow_when_the_sysfs_qgroups_directory_exists() {
    let s = sysfs("btrfs", true);
    assert_eq!(
        qgroup_note(&s.mountinfo, &s.sysfs, &s.root).unwrap(),
        "qgroups enabled: balance may be slow"
    );
    assert_eq!(QGROUP_NOTE, "qgroups enabled: balance may be slow");
}

#[test]
fn qgroup_note_should_be_absent_without_quotas_off_btrfs_and_on_unreadable_state() {
    let off = sysfs("btrfs", false);
    assert_eq!(qgroup_note(&off.mountinfo, &off.sysfs, &off.root), None);
    let ext4 = sysfs("ext4", true);
    assert_eq!(qgroup_note(&ext4.mountinfo, &ext4.sysfs, &ext4.root), None);
    assert_eq!(
        qgroup_note(Path::new("/nonexistent/mountinfo"), &off.sysfs, &off.root),
        None
    );
    assert_eq!(
        qgroup_note(&off.mountinfo, Path::new("/nonexistent/sysfs"), &off.root),
        None
    );
}

#[test]
fn report_notes_should_skip_snapper_on_macos_and_be_empty_when_nothing_is_detected() {
    let mut fs = FakeFs::new();
    fs.add_dir("/")
        .add_dir("/.snapshots")
        .add_dir("/.snapshots/5");
    let (runner, probe, clock) = (
        FakeCommandRunner::new(),
        FakeProbe::new().with_platform(Platform::Macos),
        FakeClock::at(1),
    );
    let space = FakeSpaceMeter::new(vec![], FakeSpaceMeter::stable());
    let (audit, guard, plans) = (
        FakeAuditSink::new(),
        FakeRunGuard::new(),
        FakePlanStore::new(),
    );
    let env = Env {
        fs: &fs,
        runner: &runner,
        clock: &clock,
        probe: &probe,
        space: &space,
        audit: &audit,
        guard: &guard,
        plans: &plans,
        dirs: Dirs {
            home: "/h".into(),
            state: "/h/s".into(),
            cache: "/h/c".into(),
            config: "/h/f".into(),
        },
    };
    let q = sysfs("btrfs", true);
    assert_eq!(
        report_notes_from(&env, &q.root, &q.mountinfo, &q.sysfs),
        vec![QGROUP_NOTE.to_string()],
        "macOS has no snapper layout, so only the quota note remains"
    );
    let off = sysfs("btrfs", false);
    assert!(report_notes_from(&env, &off.root, &off.mountinfo, &off.sysfs).is_empty());
    assert!(
        runner.calls().is_empty(),
        "no command is run to find snapshots or quotas"
    );
}

#[test]
fn report_notes_should_put_the_snapshot_note_before_the_qgroup_note() {
    let mut fs = FakeFs::new();
    fs.add_dir("/")
        .add_dir("/.snapshots")
        .add_dir("/.snapshots/12");
    fs.add_file(
        "/.snapshots/12/info.xml",
        "<snapshot><date>2026-09-01 09:30:00</date></snapshot>",
    );
    fs.add_dir("/.snapshots/12/snapshot");
    let (runner, probe, clock) = (FakeCommandRunner::new(), FakeProbe::new(), FakeClock::at(1));
    let space = FakeSpaceMeter::new(vec![], FakeSpaceMeter::stable());
    let (audit, guard, plans) = (
        FakeAuditSink::new(),
        FakeRunGuard::new(),
        FakePlanStore::new(),
    );
    let env = Env {
        fs: &fs,
        runner: &runner,
        clock: &clock,
        probe: &probe,
        space: &space,
        audit: &audit,
        guard: &guard,
        plans: &plans,
        dirs: Dirs {
            home: "/h".into(),
            state: "/h/s".into(),
            cache: "/h/c".into(),
            config: "/h/f".into(),
        },
    };
    let q = sysfs("btrfs", true);
    let notes = report_notes_from(&env, &q.root, &q.mountinfo, &q.sysfs);
    assert_eq!(
        notes,
        vec![
            "snapshots present (snapper: #12 2026-09-01): freed-bytes estimates may overcount shared extents".to_string(),
            QGROUP_NOTE.to_string()
        ]
    );
}

#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::path::Path;

use dunnage_adapters::snapshots::{Held, SnapshotInfo, SnapshotLayout, SnapshotSize};
use dunnage_core::ports::Env;
use dunnage_core::testing::{
    FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs, FakePlanStore, FakeProbe, FakeRunGuard,
    FakeSpaceMeter,
};
use dunnage_core::{Dirs, Platform};

struct World {
    fs: FakeFs,
    runner: FakeCommandRunner,
    probe: FakeProbe,
}

impl World {
    fn new(fs: FakeFs) -> World {
        World {
            fs,
            runner: FakeCommandRunner::new(),
            probe: FakeProbe::new(),
        }
    }

    fn detect(&self) -> Option<SnapshotInfo> {
        let clock = FakeClock::at(1);
        let space = FakeSpaceMeter::new(vec![], FakeSpaceMeter::stable());
        let (audit, guard, plans) = (
            FakeAuditSink::new(),
            FakeRunGuard::new(),
            FakePlanStore::new(),
        );
        let env = Env {
            fs: &self.fs,
            runner: &self.runner,
            clock: &clock,
            probe: &self.probe,
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
        SnapshotInfo::detect(&env)
    }
}

fn snapper_fs() -> FakeFs {
    let mut fs = FakeFs::new();
    fs.add_dir("/").add_dir("/.snapshots");
    for (n, date, desc) in [
        (13, "2026-10-01 10:00:00", "weekly"),
        (12, "2026-09-01 09:30:00", "monthly"),
    ] {
        fs.add_dir(format!("/.snapshots/{n}"));
        fs.add_file(
            format!("/.snapshots/{n}/info.xml"),
            &format!(
                "<?xml version=\"1.0\"?>\n<snapshot>\n  <type>single</type>\n  <num>{n}</num>\n  <date>{date}</date>\n  <description>{desc}</description>\n</snapshot>\n"
            ),
        );
        fs.add_dir(format!("/.snapshots/{n}/snapshot"));
    }
    fs
}

#[test]
fn snapshot_detect_should_read_snapper_and_timeshift_layouts_and_run_no_command() {
    let w = World::new(snapper_fs());
    let info = w.detect().expect("snapper layout");
    assert_eq!(info.layout, SnapshotLayout::Snapper);
    let got: Vec<_> = info
        .snapshots
        .iter()
        .map(|s| {
            (
                s.name.as_str(),
                s.date.as_deref().unwrap(),
                s.description.as_deref().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("12", "2026-09-01 09:30:00", "monthly"),
            ("13", "2026-10-01 10:00:00", "weekly")
        ]
    );
    assert!(info
        .snapshots
        .iter()
        .all(|s| s.size == SnapshotSize::Unknown));
    assert!(w.runner.calls().is_empty(), "no btrfs or any other command");

    let mut fs = FakeFs::new();
    fs.add_dir("/")
        .add_dir("/timeshift-btrfs")
        .add_dir("/timeshift-btrfs/snapshots")
        .add_dir("/timeshift-btrfs/snapshots/2026-10-01_10-00-01")
        .add_dir("/timeshift-btrfs/snapshots/2026-09-01_10-00-01");
    let w = World::new(fs);
    let info = w.detect().expect("timeshift layout");
    assert_eq!(info.layout, SnapshotLayout::Timeshift);
    let names: Vec<_> = info.snapshots.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["2026-09-01_10-00-01", "2026-10-01_10-00-01"]);
    assert!(w.runner.calls().is_empty());

    let mut fs = FakeFs::new();
    fs.add_dir("/").add_dir("/home");
    assert_eq!(World::new(fs).detect(), None);
}

#[test]
fn snapshot_detect_should_return_none_without_reading_When_platform_is_macos() {
    let mut w = World::new(snapper_fs());
    w.probe = FakeProbe::new().with_platform(Platform::Macos);
    assert_eq!(w.detect(), None);
}

#[test]
fn holds_should_answer_found_absent_or_unreadable_per_snapshot_probe() {
    let mut fs = snapper_fs();
    fs.add_dir("/.snapshots/12/snapshot/home")
        .add_dir("/.snapshots/12/snapshot/home/u")
        .add_dir("/.snapshots/12/snapshot/home/u/target");
    let w = World::new(fs);
    let info = w.detect().unwrap();
    assert_eq!(info.holds(&w.fs, Path::new("home/u/target")), Held::Found);
    assert_eq!(info.holds(&w.fs, Path::new("/home/u/target")), Held::Found);
    assert_eq!(info.holds(&w.fs, Path::new("home/u/other")), Held::Absent);

    // snapshot 13 unreadable for this user and nothing found in 12: Unreadable, not Absent
    let mut fs = snapper_fs();
    fs.fail("/.snapshots/13/snapshot/home/u/other", 13);
    let w = World::new(fs);
    let info = w.detect().unwrap();
    assert_eq!(
        info.holds(&w.fs, Path::new("home/u/other")),
        Held::Unreadable
    );
}

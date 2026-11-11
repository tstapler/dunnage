#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use disk_cleanup_core::preflight::check;
use disk_cleanup_core::testing::FakeSpaceMeter;
use disk_cleanup_core::{
    AllocSnapshot, FatalError, FsSnapshot, Kind, PreflightVerdict, SpaceUse, Starvation,
};

const MIB: u64 = 1 << 20;

fn snap(unallocated_mib: u64, headroom_mib: u64) -> FsSnapshot {
    let mut s = FakeSpaceMeter::plain_snapshot("/", 0, 0);
    s.fs_type = "btrfs".into();
    let used = SpaceUse {
        bytes_used: 0,
        total_bytes: 0,
    };
    s.btrfs = Some(AllocSnapshot {
        data: used,
        metadata: SpaceUse {
            bytes_used: 1000 * MIB,
            total_bytes: (1000 + headroom_mib) * MIB,
        },
        system: used,
        unallocated: unallocated_mib * MIB,
    });
    s
}

#[test]
fn preflight_should_refuse_with_exit_3_When_unallocated_600mib_and_headroom_32mib_and_pass_at_200mib(
) {
    let v = check(&snap(600, 32), &[Kind::Cache]);
    assert_eq!(
        v,
        PreflightVerdict::Refuse(Starvation {
            unallocated_mib: 600,
            metadata_headroom_mib: 32
        })
    );
    let fatal = v.fatal().unwrap();
    assert_eq!(
        fatal,
        FatalError::Starved {
            unallocated_mib: 600,
            metadata_headroom_mib: 32
        }
    );
    assert_eq!(fatal.exit_code(), 3);
    assert_eq!(
        check(&snap(600, 200), &[Kind::Cache]),
        PreflightVerdict::Proceed
    );
    // the AND: either figure alone is enough, and the thresholds are inclusive on the safe side
    assert_eq!(
        check(&snap(1024, 0), &[Kind::Cache]),
        PreflightVerdict::Proceed
    );
    assert_eq!(
        check(&snap(0, 64), &[Kind::Cache]),
        PreflightVerdict::Proceed
    );
    assert!(matches!(
        check(&snap(1023, 63), &[Kind::Cache]),
        PreflightVerdict::Refuse(_)
    ));
}

#[test]
fn preflight_should_not_refuse_btrfs_or_trim_kinds_When_starved_and_warn_instead() {
    let figures = Starvation {
        unallocated_mib: 600,
        metadata_headroom_mib: 32,
    };
    for kinds in [
        vec![Kind::BtrfsBalance],
        vec![Kind::BtrfsTrim],
        vec![Kind::BtrfsBalance, Kind::BtrfsTrim],
    ] {
        let v = check(&snap(600, 32), &kinds);
        assert_eq!(v, PreflightVerdict::Warn(figures));
        assert_eq!(v.fatal(), None);
    }
    // a cure mixed with a deleting kind is still refused
    assert!(matches!(
        check(&snap(600, 32), &[Kind::BtrfsBalance, Kind::Cache]),
        PreflightVerdict::Refuse(_)
    ));
}

#[test]
fn preflight_should_proceed_When_the_filesystem_is_not_btrfs() {
    let s = FakeSpaceMeter::plain_snapshot("/tmp", 10, 100);
    assert_eq!(check(&s, &[Kind::Cache]), PreflightVerdict::Proceed);
}

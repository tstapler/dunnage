#![allow(clippy::unwrap_used, non_snake_case)]

use std::path::PathBuf;

use dunnage_core::ports::RunState;
use dunnage_core::targets::build_dir::{
    kept_note, plan_keep_newest, run_state_skip, ClusterInput, KeepNewest, Placement,
    KEEP_NEWEST_ZERO_WARNING,
};
use dunnage_core::{Kind, SkipReason};

const NOW: i64 = 1_800_000_000;
const DAY: i64 = 86_400;

fn nm(name: &str, hash: Option<u64>, age_days: Option<i64>) -> ClusterInput {
    ClusterInput {
        path: PathBuf::from(format!("/h/{name}/node_modules")),
        kind: Kind::NodeModules,
        lockfile_hash: hash,
        newest_mtime: age_days.map(|d| NOW - d * DAY),
    }
}

#[test]
fn ranking_should_keep_the_1_day_old_copy_When_three_duplicates_and_keep_newest_1() {
    let v = [
        nm("b", Some(7), Some(40)),
        nm("a", Some(7), Some(1)),
        nm("c", Some(7), Some(63)),
    ];
    let p = plan_keep_newest(&v, KeepNewest(1));
    assert_eq!(
        p,
        vec![
            Placement::Offered,
            Placement::Kept { of: 3 },
            Placement::Offered
        ]
    );
    assert_eq!(kept_note(3), "kept as newest of 3 duplicates");
}

#[test]
fn unparseable_lockfile_should_be_in_no_cluster_When_hash_is_none() {
    let v = [
        nm("a", None, Some(1)),
        nm("b", None, Some(2)),
        nm("c", Some(1), Some(3)),
    ];
    let p = plan_keep_newest(&v, KeepNewest(1));
    assert_eq!(p, vec![Placement::Unclustered; 3]);
}

#[test]
fn keep_newest_zero_should_offer_every_member_and_carry_the_warning() {
    let v = [nm("a", Some(7), Some(1)), nm("b", Some(7), Some(5))];
    assert_eq!(
        plan_keep_newest(&v, KeepNewest(0)),
        vec![Placement::Offered; 2]
    );
    assert_eq!(KeepNewest(0).warning(), Some(KEEP_NEWEST_ZERO_WARNING));
    assert_eq!(
        KEEP_NEWEST_ZERO_WARNING,
        "WARNING: --keep-newest 0 also offers the newest copy of every duplicate set"
    );
    assert_eq!(KeepNewest(1).warning(), None);
}

#[test]
fn kept_row_should_never_name_keep_newest_as_the_next_action() {
    let next = SkipReason::KeptAsNewest.next_action();
    assert!(next.starts_with("none"), "{next}");
    assert!(!next.contains("--keep-newest"), "{next}");
}

#[test]
fn same_hash_should_not_cluster_across_kinds_and_a_singleton_is_unclustered() {
    let mut t = nm("a", Some(7), Some(1));
    t.kind = Kind::CargoTarget;
    let v = [t, nm("b", Some(7), Some(2)), nm("c", Some(9), Some(2))];
    assert_eq!(
        plan_keep_newest(&v, KeepNewest(1)),
        vec![Placement::Unclustered; 3]
    );
}

#[test]
fn unknown_recency_should_rank_as_newest_and_ties_should_break_by_path() {
    let v = [
        nm("b", Some(7), Some(3)),
        nm("a", Some(7), Some(3)),
        nm("z", Some(7), None),
    ];
    let p = plan_keep_newest(&v, KeepNewest(2));
    assert_eq!(
        p,
        vec![
            Placement::Offered,
            Placement::Kept { of: 3 },
            Placement::Kept { of: 3 }
        ]
    );
}

#[test]
fn keep_more_than_the_set_holds_should_keep_all() {
    let v = [nm("a", Some(7), Some(1)), nm("b", Some(7), Some(5))];
    assert_eq!(
        plan_keep_newest(&v, KeepNewest(5)),
        vec![Placement::Kept { of: 2 }; 2]
    );
}

#[test]
fn run_state_should_map_to_the_skip_reasons() {
    assert_eq!(run_state_skip(RunState::Idle), None);
    assert_eq!(
        run_state_skip(RunState::Running),
        Some(SkipReason::RunningBuild)
    );
    assert_eq!(
        run_state_skip(RunState::Unknown),
        Some(SkipReason::RunStateUnknown)
    );
}

#![allow(clippy::unwrap_used, non_snake_case)]
//! Birth time in the explorer plan identity: the comparison rule and what the plan id hashes.

use std::path::Path;

use dunnage_core::plan::{ItemIdentity, PlanId};
use dunnage_core::roots::RootIdentity;
use dunnage_core::{BirthTime, FileKind, Identity};

fn bt(secs: i64, nsec: u32) -> BirthTime {
    BirthTime { secs, nsec }
}

fn item(btime: Option<BirthTime>) -> ItemIdentity {
    ItemIdentity {
        dev: 1,
        ino: 7,
        kind: FileKind::Dir,
        btime,
    }
}

fn id(i: ItemIdentity) -> PlanId {
    let roots = [RootIdentity {
        path: "/home/u/code".into(),
        ident: Identity { dev: 1, ino: 5 },
    }];
    PlanId::for_explorer([(Path::new("/home/u/code/a"), i)], &roots)
}

#[test]
fn still_matches_should_refuse_a_recreated_directory_When_dev_ino_and_kind_are_equal_and_the_birth_times_differ(
) {
    assert!(!item(Some(bt(100, 5))).still_matches(&item(Some(bt(100, 6)))));
    assert!(!item(Some(bt(100, 5))).still_matches(&item(Some(bt(101, 5)))));
}

#[test]
fn still_matches_should_accept_the_same_object_When_the_birth_times_are_equal() {
    assert!(item(Some(bt(100, 5))).still_matches(&item(Some(bt(100, 5)))));
}

#[test]
fn still_matches_should_fall_back_to_dev_ino_and_kind_When_either_side_has_no_birth_time() {
    assert!(item(None).still_matches(&item(None)));
    assert!(item(Some(bt(1, 1))).still_matches(&item(None)));
    assert!(item(None).still_matches(&item(Some(bt(1, 1)))));
    let other_ino = ItemIdentity {
        ino: 8,
        ..item(None)
    };
    assert!(!item(None).still_matches(&other_ino));
    let other_kind = ItemIdentity {
        kind: FileKind::File,
        ..item(None)
    };
    assert!(!item(Some(bt(1, 1))).still_matches(&other_kind));
}

#[test]
fn plan_id_should_change_with_the_birth_time_When_dev_ino_and_kind_are_equal() {
    assert_ne!(id(item(Some(bt(100, 5)))), id(item(Some(bt(100, 6)))));
    assert_ne!(id(item(Some(bt(100, 5)))), id(item(Some(bt(101, 5)))));
    assert_ne!(id(item(Some(bt(100, 5)))), id(item(None)));
    assert_eq!(id(item(Some(bt(100, 5)))), id(item(Some(bt(100, 5)))));
}

#[test]
fn plan_id_should_equal_the_id_from_before_birth_times_When_the_birth_time_is_absent() {
    // Golden value computed by the pre-birth-time hash (dunnage v0.1.1, b792d5f) for this exact input.
    assert_eq!(id(item(None)).to_string(), "ad800269");
}

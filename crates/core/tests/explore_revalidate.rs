#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Painting from the cache and revalidating in place (Story 6.2.1; validation X-127, UX-T20 paint half).

#[path = "common/explore.rs"]
mod ex;
use ex::*;

use std::ffi::OsString;
use std::time::Instant;

use dunnage_core::explore::msg::{NodeEvent, ScanMode, NO_PARENT, PAINT_ID_BASE};
use dunnage_core::explore::tree::{ExploreTree, ScanStatus};
use dunnage_scan::index::DirInfo;

fn dir(id: u64, parent: u64, name: &str, alloc: u64, subdirs: u32, painted: bool) -> NodeEvent {
    NodeEvent::Dir {
        id,
        parent,
        name: OsString::from(name),
        dev: 1,
        info: DirInfo {
            stats: stats_of(alloc),
            ..DirInfo::default()
        },
        subdirs,
        from_cache: painted,
        painted,
    }
}

fn find(t: &ExploreTree, path: &str) -> Option<dunnage_core::explore::tree::NodeId> {
    t.iter()
        .find(|n| t.path_of(*n) == std::path::Path::new(path))
}

#[test]
fn a_changed_gone_and_new_directory_should_be_corrected_removed_and_attached_in_place() {
    const P: u64 = PAINT_ID_BASE;
    let mut t = ExploreTree::new();
    // cache: /r{a{x}, d, e{y}}  allocated r=1 a=10 x=100 d=1000 e=10000 y=100000
    t.apply_batch(
        0,
        ScanMode::Paint,
        vec![
            dir(P, NO_PARENT, "/r", 1, 3, true),
            dir(P + 1, P, "a", 10, 1, true),
            dir(P + 2, P + 1, "x", 100, 0, true),
            dir(P + 3, P, "d", 1000, 0, true),
            dir(P + 4, P, "e", 10000, 1, true),
            dir(P + 5, P + 4, "y", 100_000, 0, true),
        ],
    )
    .unwrap();
    let r = find(&t, "/r").unwrap();
    assert_eq!(t.total(r).allocated_bytes, 111_111);
    assert!(t.is_complete(r), "painted as a complete tree");
    assert!(t.from_cache(r));
    // live: d grew to 1500, e is gone, f is new under a, a and x unchanged
    t.apply_batch(
        0,
        ScanMode::Revalidate,
        vec![
            dir(3, 0, "d", 1500, 0, false),
            dir(2, 1, "f", 7, 0, false),
            dir(0, NO_PARENT, "/r", 1, 2, false),
            dir(1, 0, "a", 10, 2, false),
            dir(4, 1, "x", 100, 0, false),
        ],
    )
    .unwrap();
    t.finish(false);
    assert_eq!(
        t.total(find(&t, "/r").unwrap()).allocated_bytes,
        1 + 10 + 100 + 7 + 1500
    );
    assert!(
        find(&t, "/r/e").is_none() && find(&t, "/r/e/y").is_none(),
        "E and its subtree are removed"
    );
    assert_eq!(
        t.total(find(&t, "/r/a").unwrap()).allocated_bytes,
        10 + 100 + 7
    );
    assert_eq!(t.total(find(&t, "/r/d").unwrap()).allocated_bytes, 1500);
    assert!(find(&t, "/r/a/f").is_some(), "F is attached");
    assert_eq!(t.directories(), 5);
    assert_eq!(t.status(), ScanStatus::Complete);
    assert!(t.iter().all(|n| t.is_complete(n) && !t.from_cache(n)));
}

#[test]
fn a_truncated_live_pass_should_keep_unconfirmed_painted_nodes_flagged_from_cache() {
    const P: u64 = PAINT_ID_BASE;
    let mut t = ExploreTree::new();
    t.apply_batch(
        0,
        ScanMode::Paint,
        vec![
            dir(P, NO_PARENT, "/r", 1, 2, true),
            dir(P + 1, P, "a", 10, 0, true),
            dir(P + 2, P, "b", 100, 0, true),
        ],
    )
    .unwrap();
    t.apply_batch(
        0,
        ScanMode::Revalidate,
        vec![
            dir(0, NO_PARENT, "/r", 1, 2, false),
            dir(1, 0, "a", 10, 0, false),
        ],
    )
    .unwrap();
    t.finish(true);
    let b = find(&t, "/r/b").unwrap();
    assert!(t.from_cache(b), "b was never confirmed");
    assert_eq!(
        t.total(find(&t, "/r").unwrap()).allocated_bytes,
        111,
        "nothing was dropped"
    );
    assert!(!t.from_cache(find(&t, "/r/a").unwrap()));
    assert!(t.is_at_least(find(&t, "/r").unwrap()));
    assert_eq!(t.status(), ScanStatus::Truncated);
}

#[test]
fn a_directory_on_another_device_should_paint_incomplete_and_become_x_when_the_live_pass_emits_boundary(
) {
    const P: u64 = PAINT_ID_BASE;
    let mut t = ExploreTree::new();
    // the cache has no record for `mnt` (no st_dev in the cache), though the parent lists it
    t.apply_batch(
        0,
        ScanMode::Paint,
        vec![
            dir(P, NO_PARENT, "/r", 1, 2, true),
            dir(P + 1, P, "a", 10, 0, true),
        ],
    )
    .unwrap();
    let r = find(&t, "/r").unwrap();
    assert!(!t.is_complete(r), "shows `.` while a child is unknown");
    t.apply_batch(
        0,
        ScanMode::Revalidate,
        vec![
            dir(0, NO_PARENT, "/r", 1, 2, false),
            dir(1, 0, "a", 10, 0, false),
            NodeEvent::Boundary {
                id: 2,
                parent: 0,
                name: "mnt".into(),
                dev: 9,
            },
        ],
    )
    .unwrap();
    assert!(t.is_boundary(find(&t, "/r/mnt").unwrap()));
    assert!(t.is_complete(r));
    t.finish(false);
    assert_eq!(t.status(), ScanStatus::Complete);

    // truncated: it stays incomplete
    let mut t = ExploreTree::new();
    t.apply_batch(
        0,
        ScanMode::Paint,
        vec![
            dir(P, NO_PARENT, "/r", 1, 2, true),
            dir(P + 1, P, "a", 10, 0, true),
        ],
    )
    .unwrap();
    t.finish(true);
    assert!(!t.is_complete(find(&t, "/r").unwrap()));
}

#[test]
fn a_painted_directory_that_is_now_unreadable_should_lose_its_subtree_and_its_sizes() {
    const P: u64 = PAINT_ID_BASE;
    let mut t = ExploreTree::new();
    t.apply_batch(
        0,
        ScanMode::Paint,
        vec![
            dir(P, NO_PARENT, "/r", 1, 1, true),
            dir(P + 1, P, "a", 10, 1, true),
            dir(P + 2, P + 1, "b", 100, 0, true),
        ],
    )
    .unwrap();
    t.apply_batch(
        0,
        ScanMode::Revalidate,
        vec![
            dir(0, NO_PARENT, "/r", 1, 1, false),
            NodeEvent::Unreadable {
                id: 1,
                parent: 0,
                name: "a".into(),
                errno: 13,
            },
        ],
    )
    .unwrap();
    t.finish(false);
    assert_eq!(t.total(find(&t, "/r").unwrap()).allocated_bytes, 1);
    assert!(t.is_unreadable(find(&t, "/r/a").unwrap()));
    assert!(find(&t, "/r/a/b").is_none());
    assert_eq!(t.unreadable_below(find(&t, "/r").unwrap()), 1);
}

#[test]
fn _100000_live_events_for_one_painted_parent_should_apply_within_1s() {
    const P: u64 = PAINT_ID_BASE;
    const N: u64 = 100_000;
    let mut t = ExploreTree::new();
    let mut paint = vec![dir(P, NO_PARENT, "/r", 1, N as u32, true)];
    paint.extend((1..=N).map(|i| dir(P + i, P, &format!("sub{i:07}"), 5, 0, true)));
    for b in batches(paint, 1, 256) {
        t.apply_batch(0, ScanMode::Paint, b).unwrap();
    }
    let mut live = vec![dir(0, NO_PARENT, "/r", 1, N as u32, false)];
    live.extend((1..=N).map(|i| dir(i, 0, &format!("sub{i:07}"), 6, 0, false)));
    let first = live.remove(0);
    shuffle(&mut live, 99);
    let start = Instant::now();
    t.apply_batch(0, ScanMode::Revalidate, vec![first]).unwrap();
    for b in batches(live, 2, 256) {
        t.apply_batch(0, ScanMode::Revalidate, b).unwrap();
    }
    let took = start.elapsed();
    t.finish(false);
    let bound = if cfg!(debug_assertions) { 3.0 } else { 1.0 };
    assert!(took.as_secs_f64() <= bound, "took {took:?}");
    assert_eq!(t.total(find(&t, "/r").unwrap()).allocated_bytes, 1 + 6 * N);
    assert_eq!(t.directories() as u64, N + 1);
    assert_eq!(t.status(), ScanStatus::Complete);
}

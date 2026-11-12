#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! The view model (Story 6.2.2, Task 6.2.2a; validation X-045, X-046, X-047): sort keys, cursor identity and
//! the live-order policy. Time is a number of milliseconds the test advances, so nothing sleeps.

use std::ffi::OsString;

use dunnage_core::explore::layout::{layout_for_width, ColumnKind, Layout, SortMark};
use dunnage_core::explore::msg::{NodeEvent, ScanMode, NO_PARENT};
use dunnage_core::explore::tree::{ExploreTree, NodeId};
use dunnage_core::explore::view::{
    no_badge, RowId, Sort, SortKey, ViewState, QUIET_AFTER_KEY_MS, RESORT_EVERY_MS,
};
use dunnage_scan::estimate::Stats;
use dunnage_scan::index::DirInfo;
use proptest::prelude::*;

/// `files` files of `alloc` bytes allocated, `size` long, plus `subdirs` subdirectory entries.
fn own(files: u64, subdirs: u64, alloc: u64, size: u64, inodes_below: u64) -> Stats {
    Stats {
        inodes: files + subdirs + inodes_below,
        files,
        dirs: subdirs,
        dir_entries: files + subdirs,
        name_bytes: 8 * (files + subdirs),
        size_bytes: files * size,
        allocated_bytes: files * alloc,
        ..Stats::default()
    }
}

fn dir(id: u64, parent: u64, name: &str, stats: Stats) -> NodeEvent {
    NodeEvent::Dir {
        id,
        parent,
        name: OsString::from(name),
        dev: 1,
        info: DirInfo {
            stats,
            ..DirInfo::default()
        },
        // more than ever arrive, so the root stays open for later batches
        subdirs: 1_000_000,
        from_cache: false,
        painted: false,
    }
}

fn feed(t: &mut ExploreTree, ev: Vec<NodeEvent>) {
    t.apply_batch(0, ScanMode::Fresh, ev).unwrap();
}

fn child(t: &ExploreTree, parent: NodeId, name: &str) -> NodeId {
    t.children(parent).find(|c| t.name(*c) == name).unwrap()
}

fn order(v: &ViewState) -> Vec<String> {
    v.rows()
        .iter()
        .map(dunnage_core::explore::rows::Row::display_name)
        .collect()
}

/// `/r` with six children whose columns order differently.
fn six() -> ExploreTree {
    // name, files, alloc each, size each, items below
    let kids = [
        ("alpha", 1u64, 5000u64, 100u64, 0u64),
        ("bravo", 2, 3000, 900, 5),
        ("charlie", 3, 1000, 500, 2),
        ("delta", 1, 2000, 2000, 40),
        ("echo", 4, 100, 50, 1),
        ("foxtrot", 1, 1000, 700, 0),
    ];
    let mut ev = vec![dir(0, NO_PARENT, "/r", own(0, 6, 0, 0, 0))];
    for (i, (n, f, a, s, below)) in kids.iter().enumerate() {
        ev.push(dir(1 + i as u64, 0, n, own(*f, 0, *a, *s, *below)));
    }
    let mut t = ExploreTree::new();
    feed(&mut t, ev);
    t
}

fn bare(l: &str) -> String {
    l.trim_end_matches('/').to_string()
}

#[test]
fn sort_keys_should_order_by_their_column_flip_on_repeat_and_break_ties_by_name() {
    let t = six();
    let root = t.roots().next().unwrap();
    let mut v = ViewState::new(Sort::default());
    v.open_dir(&t, root, 0, &no_badge);
    let Layout::Tiered(wide) = layout_for_width(120, 30) else {
        panic!("120x30 is tiered")
    };
    let Layout::Tiered(narrow) = layout_for_width(70, 30) else {
        panic!("70x30 is tiered")
    };
    let names = |v: &ViewState| -> Vec<String> { order(v).iter().map(|s| bare(s)).collect() };

    // usage: alpha 5000, bravo 6000, charlie 3000, delta 2000, echo 400, foxtrot 1000
    assert_eq!(
        names(&v),
        ["bravo", "alpha", "charlie", "delta", "foxtrot", "echo"]
    );
    assert_eq!(v.path_segments()[0], "sort: usage, largest first");
    assert_eq!(
        v.sort_mark(&wide),
        Some(SortMark {
            column: ColumnKind::Usage,
            descending: true
        })
    );

    v.press_sort(SortKey::Usage, &t, 10, &no_badge);
    assert_eq!(
        names(&v),
        ["echo", "foxtrot", "delta", "charlie", "alpha", "bravo"]
    );
    assert_eq!(v.path_segments()[0], "sort: usage, smallest first");
    assert!(!v.sort_mark(&wide).unwrap().descending);

    // apparent: alpha 100, bravo 1800, charlie 1500, delta 2000, echo 200, foxtrot 700
    v.press_sort(SortKey::Apparent, &t, 20, &no_badge);
    assert_eq!(
        names(&v),
        ["delta", "bravo", "charlie", "foxtrot", "echo", "alpha"]
    );
    assert_eq!(v.sort_mark(&wide).unwrap().column, ColumnKind::Apparent);
    v.press_sort(SortKey::Apparent, &t, 30, &no_badge);
    assert_eq!(
        names(&v),
        ["alpha", "echo", "foxtrot", "charlie", "bravo", "delta"]
    );

    // items (the directory itself and what is below): alpha 2, bravo 8, charlie 6, delta 42, echo 6, foxtrot 2
    // two ties: alpha/foxtrot and charlie/echo, broken by name whatever the direction
    v.press_sort(SortKey::Items, &t, 40, &no_badge);
    assert_eq!(
        names(&v),
        ["delta", "bravo", "charlie", "echo", "alpha", "foxtrot"]
    );
    v.press_sort(SortKey::Items, &t, 50, &no_badge);
    assert_eq!(
        names(&v),
        ["alpha", "foxtrot", "charlie", "echo", "bravo", "delta"]
    );
    assert_eq!(v.path_segments()[0], "sort: items, fewest first");

    // metadata is a size estimate from the totals: more entries and bytes means more
    v.press_sort(SortKey::Meta, &t, 60, &no_badge);
    assert_eq!(v.path_segments()[0], "sort: metadata, largest first");
    let metas: Vec<u64> = v.rows().iter().map(|r| r.meta.unwrap()).collect();
    assert!(metas.windows(2).all(|w| w[0] >= w[1]), "{metas:?}");
    v.press_sort(SortKey::Meta, &t, 70, &no_badge);
    let metas: Vec<u64> = v.rows().iter().map(|r| r.meta.unwrap()).collect();
    assert!(metas.windows(2).all(|w| w[0] <= w[1]), "{metas:?}");

    // name ascending first, then reversed
    v.press_sort(SortKey::Name, &t, 80, &no_badge);
    assert_eq!(
        names(&v),
        ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"]
    );
    assert_eq!(v.path_segments()[0], "sort: name, A to Z");
    assert_eq!(
        v.sort_mark(&wide),
        Some(SortMark {
            column: ColumnKind::Name,
            descending: false
        })
    );
    v.press_sort(SortKey::Name, &t, 90, &no_badge);
    assert_eq!(
        names(&v),
        ["foxtrot", "echo", "delta", "charlie", "bravo", "alpha"]
    );

    // a column hidden at the narrow tier still sorts, and the title says so
    v.press_sort(SortKey::Apparent, &t, 100, &no_badge);
    assert_eq!(
        names(&v),
        ["delta", "bravo", "charlie", "foxtrot", "echo", "alpha"]
    );
    assert_eq!(v.sort_mark(&narrow), None);
    assert_eq!(
        v.hidden_column_note(&narrow).as_deref(),
        Some("sorted by APPARENT (column hidden at this width)")
    );
    assert_eq!(v.hidden_column_note(&wide), None);
    v.press_sort(SortKey::Usage, &t, 110, &no_badge);
    assert_eq!(
        v.hidden_column_note(&narrow),
        None,
        "usage is drawn at every tier"
    );
}

#[test]
fn resort_should_run_at_most_once_a_second_and_never_within_2s_of_a_key() {
    assert_eq!((RESORT_EVERY_MS, QUIET_AFTER_KEY_MS), (1_000, 2_000));
    let mut t = ExploreTree::new();
    feed(
        &mut t,
        vec![
            dir(0, NO_PARENT, "/r", own(0, 2, 0, 0, 0)),
            dir(1, 0, "a", own(1, 0, 1000, 1000, 0)),
            dir(2, 0, "b", own(1, 0, 500, 500, 0)),
        ],
    );
    let root = t.roots().next().unwrap();
    let (a, b) = (child(&t, root, "a"), child(&t, root, "b"));
    let mut v = ViewState::new(Sort::default());
    v.open_dir(&t, root, 0, &no_badge); // a key at t = 0 and a sort at t = 0
    v.move_to(0, 0);
    let ids = |v: &ViewState| -> Vec<RowId> { v.rows().iter().map(|r| r.id.clone()).collect() };
    assert_eq!(ids(&v), [RowId::Dir(a), RowId::Dir(b)]);
    assert_eq!(v.cursor(), Some(&RowId::Dir(a)));

    // b grows past a: a grandchild of 5,000 bytes arrives under b
    feed(&mut t, vec![dir(3, 2, "big", own(1, 0, 5000, 5000, 0))]);

    // 0.5 s after the last sort: not due; the number moves in place, the order does not
    assert!(!v.order_snapshot(&t, 500, &no_badge));
    assert_eq!(ids(&v), [RowId::Dir(a), RowId::Dir(b)]);
    assert_eq!(v.rows()[1].usage, 5500, "sizes update in place");
    // 1 s after the last sort but only 1 s after the key at t = 0: not due
    assert!(!v.order_snapshot(&t, 1_000, &no_badge));
    assert_eq!(ids(&v), [RowId::Dir(a), RowId::Dir(b)]);
    assert!(
        !v.order_snapshot(&t, 1_999, &no_badge),
        "1.999 s of silence is not 2 s"
    );
    // 2 s of silence: the next tick moves the rows, the cursor keeps its item
    assert!(v.order_snapshot(&t, 2_000, &no_badge));
    assert_eq!(ids(&v), [RowId::Dir(b), RowId::Dir(a)]);
    assert_eq!(v.cursor(), Some(&RowId::Dir(a)));
    assert_eq!(
        v.cursor_index(),
        1,
        "the cursor row moved down with its item"
    );

    // a row that arrives between re-sorts goes to the end, even when it is the largest
    feed(
        &mut t,
        vec![dir(4, 0, "huge", own(1, 0, 90_000, 90_000, 0))],
    );
    assert!(
        !v.order_snapshot(&t, 2_100, &no_badge),
        "100 ms after a sort"
    );
    let last = v.rows().last().unwrap();
    assert_eq!(last.id, RowId::Dir(child(&t, root, "huge")));
    assert_eq!(v.rows().len(), 3);

    // a key at 2,500 holds the order for two more seconds; the next re-sort is at 4,500 or later
    v.move_cursor(-1, 2_500);
    feed(&mut t, vec![dir(5, 1, "more", own(1, 0, 1, 1, 0))]);
    assert!(!v.order_snapshot(&t, 3_500, &no_badge), "1 s after a key");
    assert!(!v.order_snapshot(&t, 4_499, &no_badge));
    assert!(v.order_snapshot(&t, 4_500, &no_badge));
    assert_eq!(v.rows()[0].id, RowId::Dir(child(&t, root, "huge")));
    assert_eq!(
        v.cursor(),
        Some(&RowId::Dir(b)),
        "move_cursor(-1) chose b and it stayed chosen"
    );
    // and then at most once a second: a tick 0.5 s later does not re-sort even in silence
    assert!(!v.order_snapshot(&t, 5_000, &no_badge));
    assert!(v.order_snapshot(&t, 5_500, &no_badge));
}

const KIDS: u64 = 40;

proptest! {
    #![proptest_config(ProptestConfig { cases: 6, ..ProptestConfig::default() })]

    #[test]
    fn cursor_should_keep_its_node_identity_across_1000_random_batches_and_resorts(seed in 1u64..u64::MAX) {
        let mut rng = seed | 1;
        let mut next = move |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        let mut t = ExploreTree::new();
        let mut ev = vec![dir(0, NO_PARENT, "/r", own(0, KIDS, 0, 0, 0))];
        for i in 0..KIDS {
            ev.push(dir(1 + i, 0, &format!("k{i:02}"), own(1, 0, 4096 * (1 + next(50)), 100, 0)));
        }
        feed(&mut t, ev);
        let root = t.roots().next().unwrap();
        let mut v = ViewState::new(Sort::default());
        v.open_dir(&t, root, 0, &no_badge);
        let at = next(KIDS) as usize;
        v.move_to(at, 0);
        let chosen = v.cursor().cloned().unwrap();
        let RowId::Dir(chosen_node) = chosen.clone() else { panic!("a directory row") };

        let mut now = 0u64;
        let mut resorts = 0;
        for batch in 0..1_000u64 {
            // a batch grows one random directory with a new grandchild
            let parent = 1 + next(KIDS);
            feed(&mut t, vec![dir(1 + KIDS + batch, parent, &format!("g{batch}"), own(1, 0, 4096 * (1 + next(500)), 100, 0))]);
            now += next(1_500);
            if next(9) == 0 {
                // the owner presses a key that does not move the cursor
                v.note_key(now);
            }
            if v.order_snapshot(&t, now, &no_badge) {
                resorts += 1;
            }
            prop_assert_eq!(v.cursor(), Some(&chosen));
            prop_assert_eq!(v.activate(), Some(chosen_node));
            prop_assert_eq!(&v.cursor_row().unwrap().id, &chosen);
        }
        prop_assert!(resorts > 10, "the property must exercise re-sorts, saw {}", resorts);
    }
}

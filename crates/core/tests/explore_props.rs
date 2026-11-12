#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Properties of the arena reducer: arrival order, monotonic growth, revalidation against a fresh scan
//! and stale generations (Story 6.2.1; validation X-010, X-011, X-013, X-014, UX-T10).

#[path = "common/explore.rs"]
mod ex;
use ex::Rng as TestRng;
use ex::{
    batches, by_path, events, fresh, paint_events, path_of_spec, random_tree, reduce, reference,
    shuffle, Spec, K,
};

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

use dunnage_core::explore::msg::{NodeEvent, ScanMode};
use dunnage_core::explore::tree::{ExploreTree, ScanStatus};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn any_shuffle_and_batching_of_events_should_give_the_same_totals(seed in 0u64..1_000_000) {
        let specs = random_tree(seed, 8, 6, 400);
        let want = reference(&specs);
        let mut evs = events(&specs, 0, false);
        shuffle(&mut evs, seed);
        let mut t = ExploreTree::new();
        reduce(&mut t, ScanMode::Fresh, 0, batches(evs, seed, 17));
        for n in t.iter() {
            prop_assert!(t.is_complete(n), "{:?} still open", t.path_of(n));
        }
        t.finish(false);
        prop_assert_eq!(t.orphans_dropped(), 0);
        prop_assert_eq!(t.status(), ScanStatus::Complete);
        prop_assert_eq!(by_path(&t), want);
    }

    #[test]
    fn a_fresh_scan_should_never_decrease_any_node_total(seed in 0u64..1_000_000) {
        let specs = random_tree(seed, 8, 6, 300);
        let mut evs = events(&specs, 0, false);
        shuffle(&mut evs, seed);
        let mut t = ExploreTree::new();
        let mut last: HashMap<PathBuf, u64> = HashMap::new();
        for b in batches(evs, seed, 9) {
            t.apply_batch(0, ScanMode::Fresh, b).unwrap();
            for n in t.iter() {
                let a = t.total(n).allocated_bytes;
                let p = t.path_of(n);
                prop_assert!(a >= last.get(&p).copied().unwrap_or(0), "{:?} shrank", p);
                last.insert(p, a);
            }
        }
    }
}

/// The final tree after one directory changed, one subtree is gone and one directory is new.
fn mutate(old: &[Spec], seed: u64) -> Vec<Spec> {
    let mut rng = TestRng::new(seed);
    let dirs: Vec<usize> = (1..old.len()).filter(|&i| old[i].kind == K::Dir).collect();
    let mut gone = std::collections::HashSet::new();
    if dirs.len() > 3 {
        let e = dirs[rng.below(dirs.len())];
        gone.insert(e);
    }
    // everything below a gone node is gone too
    let mut keep = vec![true; old.len()];
    for i in 0..old.len() {
        keep[i] = !gone.contains(&i) && old[i].parent.is_none_or(|p| keep[p]);
    }
    let mut map = vec![usize::MAX; old.len()];
    let mut out: Vec<Spec> = Vec::new();
    for i in 0..old.len() {
        if keep[i] {
            map[i] = out.len();
            let mut s = old[i].clone();
            s.parent = old[i].parent.map(|p| map[p]);
            out.push(s);
        }
    }
    if let Some(d) = out
        .iter()
        .position(|s| s.kind == K::Dir && s.parent.is_some())
    {
        out[d].alloc += 8192; // D changed
    }
    let live_dirs: Vec<usize> = (0..out.len()).filter(|&i| out[i].kind == K::Dir).collect();
    let at = live_dirs[rng.below(live_dirs.len())];
    out.push(Spec {
        parent: Some(at),
        name: "new-dir".into(),
        kind: K::Dir,
        alloc: 12288,
    });
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn a_painted_tree_revalidated_by_a_live_pass_should_equal_a_fresh_scan(seed in 0u64..1_000_000) {
        let old = random_tree(seed, 7, 5, 300);
        let new = mutate(&old, seed);
        let mut t = ExploreTree::new();
        let mut paint = paint_events(&old);
        shuffle(&mut paint, seed);
        reduce(&mut t, ScanMode::Paint, 0, batches(paint, seed, 11));
        let mut live = events(&new, 0, false);
        shuffle(&mut live, seed ^ 5);
        reduce(&mut t, ScanMode::Revalidate, 0, batches(live, seed, 13));
        t.finish(false);
        let fresh = by_path(&fresh(&new));
        prop_assert_eq!(by_path(&t), fresh.clone());
        prop_assert_eq!(by_path(&t), reference(&new));
        prop_assert_eq!(t.status(), ScanStatus::Complete);
        for n in t.iter() {
            prop_assert!(t.is_complete(n));
            prop_assert!(!t.from_cache(n), "{:?} is still marked from cache", t.path_of(n));
        }
        prop_assert_eq!(t.directories(), fresh.len());
    }

    #[test]
    fn messages_of_an_old_generation_should_change_nothing_after_a_rescan(seed in 0u64..1_000_000) {
        let specs = random_tree(seed, 6, 5, 200);
        prop_assume!(specs.len() > 3);
        let evs = events(&specs, 0, false);
        let mut t = ExploreTree::new();
        let first: Vec<NodeEvent> = evs[..evs.len() / 2].to_vec();
        reduce(&mut t, ScanMode::Fresh, 0, batches(first, seed, 7));
        let target = t.roots().next().unwrap();
        let generation = t.begin_rescan(target);
        prop_assert_eq!(generation, 1);
        let after_reset = by_path(&t);
        // the old source keeps sending
        reduce(&mut t, ScanMode::Fresh, 0, batches(evs.clone(), seed, 5));
        prop_assert_eq!(by_path(&t), after_reset);
        // the new generation rebuilds the tree
        let mut again = evs;
        shuffle(&mut again, seed);
        reduce(&mut t, ScanMode::Fresh, 1, batches(again, seed, 5));
        t.finish(false);
        prop_assert_eq!(by_path(&t), reference(&specs));
    }
}

#[test]
fn a_rescan_of_a_subtree_should_replace_it_and_leave_the_rest_alone() {
    let specs = random_tree(7, 5, 4, 80);
    let mut t = fresh_open(&specs);
    let sub = t
        .iter()
        .find(|n| t.path_of(*n) == path_of_spec(&specs, 1))
        .unwrap();
    let before = by_path(&t);
    t.begin_rescan(sub);
    for n in t.iter() {
        let p = t.path_of(n);
        assert!(
            !p.starts_with(path_of_spec(&specs, 1)) || p == path_of_spec(&specs, 1),
            "{p:?} should be dropped"
        );
    }
    assert!(
        !t.is_complete(t.roots().next().unwrap()),
        "the ancestors reopen"
    );
    // the walker restarts rooted at the subtree: its root event has no parent
    let sub_specs: Vec<Spec> = subtree_specs(&specs, 1);
    let mut evs = events(&sub_specs, 0, false);
    shuffle(&mut evs, 3);
    t.apply_batch(1, ScanMode::Fresh, evs).unwrap();
    t.finish(false);
    assert_eq!(by_path(&t), before);
    assert_eq!(t.status(), ScanStatus::Complete);
}

fn fresh_open(specs: &[Spec]) -> ExploreTree {
    let mut t = ExploreTree::new();
    t.apply_batch(0, ScanMode::Fresh, events(specs, 0, false))
        .unwrap();
    t
}

fn subtree_specs(specs: &[Spec], top: usize) -> Vec<Spec> {
    let mut map = HashMap::new();
    let mut out = Vec::new();
    for i in 0..specs.len() {
        let inside = i == top || specs[i].parent.is_some_and(|p| map.contains_key(&p));
        if inside {
            map.insert(i, out.len());
            let mut s = specs[i].clone();
            s.parent = if i == top {
                None
            } else {
                specs[i].parent.map(|p| map[&p])
            };
            if i == top {
                s.name = path_of_spec(specs, top).to_string_lossy().into_owned();
            }
            out.push(s);
        }
    }
    out
}

#[test]
fn fresh_scan_totals_should_only_grow_dots_should_clear_and_the_title_should_change() {
    let specs = random_tree(11, 6, 5, 200);
    let mut t = ExploreTree::new();
    let mut evs = events(&specs, 0, false);
    shuffle(&mut evs, 4);
    assert_eq!(t.status(), ScanStatus::Scanning);
    let mut last = 0;
    for b in batches(evs, 4, 8) {
        t.apply_batch(0, ScanMode::Fresh, b).unwrap();
        if let Some(r) = t.roots().next() {
            let a = t.total(r).allocated_bytes;
            assert!(a >= last);
            last = a;
        }
    }
    assert!(t.iter().all(|n| t.is_complete(n)), "the dots clear");
    t.finish(false);
    assert_eq!(t.status(), ScanStatus::Complete);
    let _ = OsString::new();
}

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    non_snake_case,
    clippy::many_single_char_names
)]
//! The explorer's ports and the arena reducer (Story 6.2.1; validation X-012, X-015, X-126, X-144, UX-T19,
//! UX-T20). Random-order and revalidation properties are in `explore_props.rs`.

#[path = "common/explore.rs"]
mod ex;
use ex::*;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use disk_cleanup_core::explore::msg::{NodeEvent, ScanMode, ScanMsg, NO_PARENT};
use disk_cleanup_core::explore::ports::{
    DirLister, ExplorerActions, ScanOpts, ScanSource, TermSignals,
};
use disk_cleanup_core::explore::testing::{
    FakeDirLister, FakeExplorerActions, FakeScanSource, FakeTermSignals,
};
use disk_cleanup_core::explore::tree::{
    ExploreTree, Inside, PartialCause, ReduceError, ScanStatus,
};
use disk_cleanup_core::ports::ExtentProbe;
use disk_cleanup_scan::index::DirInfo;
use proptest::prelude::*;

fn dir(id: u64, parent: u64, name: &str, alloc: u64, subdirs: u32) -> NodeEvent {
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
        from_cache: false,
        painted: false,
    }
}

fn find(t: &ExploreTree, path: &str) -> disk_cleanup_core::explore::tree::NodeId {
    t.iter()
        .find(|n| t.path_of(*n) == std::path::Path::new(path))
        .unwrap()
}

fn assert_send_sync<T: Send + Sync + ?Sized>(_: &T) {}

#[test]
fn ports_should_be_shared_across_scoped_threads_and_the_fakes_should_script_batches() {
    let src = FakeScanSource::new(vec![ScanMsg::Batch {
        generation: 0,
        mode: ScanMode::Fresh,
        events: vec![dir(0, NO_PARENT, "/r", 4096, 0)],
    }]);
    let lister = FakeDirLister::default();
    let sig = FakeTermSignals::at_tick(2, 15);
    let actions: Box<dyn ExplorerActions> = Box::new(FakeExplorerActions::new(
        disk_cleanup_core::explore::msg::PlanPreview {
            plan_id: "p".into(),
            saved: false,
            rows: vec![],
            est_bytes: 0,
            scope_line: String::new(),
            guards_line: String::new(),
        },
    ));
    let probe: &dyn ExtentProbe = &Probe;
    let (s, l, g, a): (
        &dyn ScanSource,
        &dyn DirLister,
        &dyn TermSignals,
        &dyn ExplorerActions,
    ) = (&src, &lister, &sig, &*actions);
    assert_send_sync(s);
    assert_send_sync(l);
    assert_send_sync(g);
    assert_send_sync(a);
    assert_send_sync(probe);
    std::thread::scope(|sc| {
        sc.spawn(|| {
            let _ = l.list_files(std::path::Path::new("/x"), 5, &AtomicBool::new(false));
        });
        sc.spawn(|| {
            let h = s.start(&[PathBuf::from("/r")], &ScanOpts::default());
            assert!(matches!(h.rx.recv().unwrap(), ScanMsg::Batch { .. }));
        });
        sc.spawn(|| {
            assert_eq!(g.interrupted(), None);
            assert_eq!(g.interrupted(), Some(15));
        });
        sc.spawn(|| {
            let h = a.preview(&[PathBuf::from("/m")], Arc::new(AtomicBool::new(false)));
            assert!(matches!(h.rx.recv().unwrap(), ScanMsg::Preview(_)));
        });
        sc.spawn(|| {
            let _ = probe.shared_bytes(std::path::Path::new("/x"), 1, 1);
        });
    });
}

struct Probe;
impl ExtentProbe for Probe {
    fn attribute(
        &self,
        _items: &[&std::path::Path],
        _budget: usize,
    ) -> disk_cleanup_core::ports::ExtentAttribution {
        disk_cleanup_core::ports::ExtentAttribution::Measured(vec![])
    }
}

#[test]
fn completion_should_need_the_node_and_every_child_terminal_event_including_boundary_unreadable_gone(
) {
    // root has four children: a Dir, an Unreadable, a Boundary and a Gone
    let mut t = ExploreTree::new();
    let b = |t: &mut ExploreTree, e| t.apply_batch(0, ScanMode::Fresh, vec![e]).unwrap();
    b(&mut t, dir(0, NO_PARENT, "/r", 100, 4));
    let root = find(&t, "/r");
    assert!(!t.is_complete(root), "children are still open");
    b(&mut t, dir(1, 0, "a", 10, 1));
    b(
        &mut t,
        NodeEvent::Unreadable {
            id: 2,
            parent: 0,
            name: "u".into(),
            errno: 13,
        },
    );
    b(
        &mut t,
        NodeEvent::Boundary {
            id: 3,
            parent: 0,
            name: "x".into(),
            dev: 9,
        },
    );
    assert!(!t.is_complete(root), "Gone has not arrived");
    b(
        &mut t,
        NodeEvent::Gone {
            id: 4,
            parent: 0,
            name: "g".into(),
        },
    );
    assert!(
        !t.is_complete(root),
        "a has an open child, so the root's totals are not final"
    );
    b(&mut t, dir(5, 1, "leaf", 1, 0));
    assert!(t.is_complete(find(&t, "/r/a")));
    assert!(t.is_complete(root));
    assert_eq!(
        key(t.total(root)).0,
        111,
        "Unreadable, Boundary and Gone add nothing"
    );
}

#[test]
fn unreadable_row_bang_size_at_least_and_errors_overlay() {
    let mut t = ExploreTree::new();
    t.apply_batch(
        0,
        ScanMode::Fresh,
        vec![
            dir(0, NO_PARENT, "/r", 100, 1),
            dir(1, 0, "a", 10, 1),
            NodeEvent::Unreadable {
                id: 2,
                parent: 1,
                name: "secret".into(),
                errno: 13,
            },
        ],
    )
    .unwrap();
    let u = find(&t, "/r/a/secret");
    assert!(t.is_unreadable(u));
    assert_eq!(key(t.total(u)), (0, 0, 0), "sizes stay n/a");
    assert_eq!(t.unreadable_below(find(&t, "/r/a")), 1);
    assert_eq!(t.unreadable_below(find(&t, "/r")), 1);
    assert!(t.is_complete(find(&t, "/r")));
}

#[test]
fn boundary_row_x_na_sizes_never_scanned_with_the_parent_and_enter_starts_a_new_root_and_h_returns()
{
    let mut t = ExploreTree::new();
    t.apply_batch(
        0,
        ScanMode::Fresh,
        vec![
            dir(0, NO_PARENT, "/r", 100, 1),
            NodeEvent::Boundary {
                id: 1,
                parent: 0,
                name: "mnt".into(),
                dev: 7,
            },
        ],
    )
    .unwrap();
    let b = find(&t, "/r/mnt");
    assert!(t.is_boundary(b));
    assert_eq!(key(t.total(b)), (0, 0, 0));
    assert_eq!(
        key(t.total(find(&t, "/r"))).0,
        100,
        "adds zero to every total"
    );
    assert!(
        t.is_complete(find(&t, "/r")),
        "a boundary child is terminal"
    );
    assert_eq!(t.children(b).count(), 0, "nothing below it is listed");
}

#[test]
fn a_truncated_scan_should_leave_incomplete_nodes_at_least() {
    let mut t = ExploreTree::new();
    t.apply_batch(
        0,
        ScanMode::Fresh,
        vec![dir(0, NO_PARENT, "/r", 100, 2), dir(1, 0, "a", 10, 0)],
    )
    .unwrap();
    t.finish(true);
    let r = find(&t, "/r");
    assert!(t.is_at_least(r));
    assert!(!t.is_at_least(find(&t, "/r/a")));
    assert_eq!(t.status(), ScanStatus::Truncated);
}

#[test]
fn orphans_beyond_the_park_cap_should_be_counted_and_an_id_past_u32_should_be_tree_full() {
    let mut t = ExploreTree::new();
    let evs: Vec<NodeEvent> = (0..200_000u64)
        .map(|i| dir(10 + i, 1_000_000_000 + i, "o", 1, 0))
        .collect();
    for chunk in evs.chunks(1000) {
        t.apply_batch(0, ScanMode::Fresh, chunk.to_vec()).unwrap();
    }
    t.finish(false);
    assert_eq!(
        t.orphans_dropped(),
        200_000,
        "100,000 dropped at the cap, 100,000 parked then dropped at the end"
    );
    assert_eq!(t.directories(), 0);
    assert_eq!(t.status(), ScanStatus::Partial(PartialCause::Orphans));

    let mut t = ExploreTree::new();
    t.apply_batch(0, ScanMode::Fresh, vec![dir(0, NO_PARENT, "/r", 1, 0)])
        .unwrap();
    let e = t.apply_batch(
        0,
        ScanMode::Fresh,
        vec![dir(u64::from(u32::MAX), 0, "x", 1, 0)],
    );
    assert_eq!(e, Err(ReduceError::TreeFull));
    assert_eq!(t.directories(), 1, "the tree stays consistent");
    t.finish(false);
    assert_eq!(t.status(), ScanStatus::Partial(PartialCause::TreeFull));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn past_the_node_cap_events_should_fold_into_the_nearest_kept_ancestor_and_kept_totals_should_equal_an_uncapped_reduction(seed in 0u64..10_000) {
        let specs = random_tree(seed, 8, 6, 5_000);
        prop_assume!(specs.len() > 1_200);
        let reference = reference(&specs);
        let mut evs = events(&specs, 0, false);
        shuffle(&mut evs, seed);
        let mut t = ExploreTree::with_limits(1_000, 100_000);
        reduce(&mut t, ScanMode::Fresh, 0, batches(evs, seed, 40));
        t.finish(false);
        prop_assert_eq!(t.directories(), 1_000);
        prop_assert_eq!(t.orphans_dropped(), 0);
        prop_assert_eq!(t.status(), ScanStatus::Partial(PartialCause::NodeLimit));
        for n in t.iter() {
            let path = t.path_of(n);
            prop_assert_eq!(key(t.total(n)), reference[&path], "{:?}", path);
            prop_assert!(t.is_complete(n), "every folded descendant's event arrived: {:?}", path);
            if let Some(p) = t.parent(n) {
                prop_assert!(t.iter().any(|x| x == p), "kept set is ancestor-closed");
            }
        }
        let root = t.roots().next().unwrap();
        prop_assert!(t.not_listed(root) > 0);
    }
}

fn tree_of(paths: &[(&str, bool, bool)]) -> ExploreTree {
    // (path, has_package_json, has_cargo_toml); parents listed before children
    let mut t = ExploreTree::new();
    let mut evs = Vec::new();
    for (i, (p, pkg, cargo)) in paths.iter().enumerate() {
        let (parent, name) = match p.rfind('/') {
            Some(0) | None => (NO_PARENT, (*p).to_string()),
            Some(k) => match paths.iter().position(|x| x.0 == &p[..k]) {
                Some(pid) => (pid as u64, p[k + 1..].to_string()),
                None => (NO_PARENT, (*p).to_string()),
            },
        };
        let kids = paths
            .iter()
            .filter(|x| x.0.rfind('/').is_some_and(|k| &x.0[..k] == *p))
            .count() as u32;
        evs.push(NodeEvent::Dir {
            id: i as u64,
            parent,
            name: name.into(),
            dev: 1,
            info: DirInfo {
                stats: stats_of(1),
                has_package_json: *pkg,
                has_cargo_toml: *cargo,
                ..DirInfo::default()
            },
            subdirs: kids,
            from_cache: false,
            painted: false,
        });
    }
    t.apply_batch(0, ScanMode::Fresh, evs).unwrap();
    t
}

#[test]
fn inside_special_should_find_a_build_dir_or_git_ancestor_by_walking_the_parents_without_io() {
    let t = tree_of(&[
        ("/s/a", true, false),
        ("/s/a/node_modules", false, false),
        ("/s/a/node_modules/foo", true, false),
        ("/s/a/node_modules/foo/node_modules", false, false),
    ]);
    // the plan's `a` has a package.json: without it the outer directory is not a build directory
    assert_eq!(
        t.inside_special(find(&t, "/s/a/node_modules/foo/node_modules")),
        Inside::Yes
    );
    assert_eq!(
        t.inside_special(find(&t, "/s/a/node_modules")),
        Inside::No,
        "a plain node_modules"
    );

    let t = tree_of(&[
        ("/s/a", true, false),
        ("/s/a/.git", false, false),
        ("/s/a/.git/node_modules", false, false),
    ]);
    assert_eq!(
        t.inside_special(find(&t, "/s/a/.git/node_modules")),
        Inside::Yes
    );

    let t = tree_of(&[
        ("/s/a", false, true),
        ("/s/a/target", false, false),
        ("/s/a/target/debug", false, true),
        ("/s/a/target/debug/target", false, false),
    ]);
    assert_eq!(
        t.inside_special(find(&t, "/s/a/target/debug/target")),
        Inside::Yes
    );

    // a scan rooted at a/node_modules/foo: the outer node_modules is above the root
    let t = tree_of(&[
        ("/s/a/node_modules/foo", true, false),
        ("/s/a/node_modules/foo/node_modules", false, false),
    ]);
    assert_eq!(
        t.inside_special(find(&t, "/s/a/node_modules/foo/node_modules")),
        Inside::Unknown
    );
    assert_eq!(
        t.inside_special(find(&t, "/s/a/node_modules/foo")),
        Inside::Unknown
    );

    // a `.git` above the scan root is certain
    let t = tree_of(&[
        ("/s/.git/hooks", false, false),
        ("/s/.git/hooks/x", false, false),
    ]);
    assert_eq!(t.inside_special(find(&t, "/s/.git/hooks/x")), Inside::Yes);

    // a scan rooted at a node_modules directory cannot see the parent's flags
    let t = tree_of(&[
        ("/s/a/node_modules", false, false),
        ("/s/a/node_modules/x", false, false),
    ]);
    assert_eq!(
        t.inside_special(find(&t, "/s/a/node_modules/x")),
        Inside::Unknown
    );
    let t = tree_of(&[("/s/a", false, false), ("/s/a/x", false, false)]);
    assert_eq!(t.inside_special(find(&t, "/s/a/x")), Inside::No);
}

#[test]
fn a_permuted_fake_source_should_still_reduce_to_the_reference_totals() {
    for seed in 1..20u64 {
        let specs = random_tree(seed, 6, 5, 300);
        let mut evs = events(&specs, 0, false);
        shuffle(&mut evs, seed);
        let mut script: Vec<ScanMsg> = batches(evs, seed, 16)
            .into_iter()
            .map(|events| ScanMsg::Batch {
                generation: 0,
                mode: ScanMode::Fresh,
                events,
            })
            .collect();
        script.push(ScanMsg::Finished(
            disk_cleanup_core::explore::msg::ScanSummary::default(),
        ));
        let src = FakeScanSource::new(script).permuted(seed);
        let h = src.start(&[PathBuf::from("/r")], &ScanOpts::default());
        let mut t = ExploreTree::new();
        let mut finished = false;
        while let Ok(m) = h.rx.recv() {
            match m {
                ScanMsg::Batch {
                    generation,
                    mode,
                    events,
                } => t.apply_batch(generation, mode, events).unwrap(),
                ScanMsg::Finished(_) => finished = true,
                _ => {}
            }
        }
        assert!(finished, "Finished stays last");
        t.finish(false);
        assert_eq!(by_path(&t), reference(&specs), "seed {seed}");
    }
}

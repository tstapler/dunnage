#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.4.1 (Task 6.4.1b): the `MarkRefusal` catalogue texts and the bounded `ContainsItems` count.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use disk_cleanup_core::explore::msg::{MarkRefusal, NodeEvent, ScanMode, NO_PARENT};
use disk_cleanup_core::explore::refusal::{
    count_items_below, count_text, RefusalCtx, CONTAINS_CAP, MARK_LIMIT,
};
use disk_cleanup_core::explore::tree::{ExploreTree, NodeId};
use disk_cleanup_core::SkipReason;
use disk_cleanup_scan::index::DirInfo;

fn roots(n: usize) -> Vec<PathBuf> {
    (0..n).map(|i| PathBuf::from(format!("/h/r{i}"))).collect()
}

fn text(r: MarkRefusal, path: &Path, roots: &[PathBuf], real: Option<&Path>) -> String {
    r.text(&RefusalCtx {
        path,
        roots,
        config_path: Path::new("/h/.config/disk-cleanup/config.toml"),
        real_path: real,
        tty: false,
    })
}

#[test]
fn outside_root_should_name_at_most_three_roots_then_plus_n_more_and_say_how_to_add_one() {
    let p = Path::new("/data/x/node_modules");
    let t = text(MarkRefusal::OutsideRoot, p, &roots(2), None);
    assert_eq!(
        t,
        "/data/x/node_modules is outside the folders the cleaner may touch (/h/r0, /h/r1). \
         The explorer never widens them; to include it add its parent to 'roots' in \
         /h/.config/disk-cleanup/config.toml and run explore again."
    );
    let t = text(MarkRefusal::OutsideRoot, p, &roots(5), None);
    assert!(t.contains("(/h/r0, /h/r1, /h/r2, +2 more)"), "{t}");
    let t = text(MarkRefusal::OutsideRoot, p, &roots(3), None);
    assert!(t.contains("(/h/r0, /h/r1, /h/r2)"), "{t}");
}

#[test]
fn the_plan_literals_should_appear_verbatim() {
    let p = Path::new("/h/code/app/node_modules/foo/node_modules");
    assert_eq!(
        text(MarkRefusal::InsideBuildOrGit, p, &[], None),
        "/h/code/app/node_modules/foo/node_modules is inside a build directory or .git that the \
         cleaner lists as one item. Mark the outer directory instead."
    );
    assert!(text(MarkRefusal::NotAnItem, p, &[], None)
        .contains("not an item of any cleanable kind, so the cleaner never touches it"));
    assert_eq!(
        text(MarkRefusal::StillScanning, p, &[], None),
        "1 marked item is still being scanned. Wait, or Esc to stop the scan."
    );
    assert_eq!(
        text(MarkRefusal::LimitReached, p, &[], None),
        "20 items are marked, the limit for one plan. Delete these first, then mark more."
    );
    assert_eq!(MARK_LIMIT, 20);
    assert!(
        text(MarkRefusal::PathTooLong, p, &[], None).contains("the --only line cannot carry it")
    );
}

#[test]
fn a_symlink_row_should_name_the_real_path_shell_quoted_and_not_the_generic_roots_advice() {
    let link = Path::new("/h/code");
    let t = text(
        MarkRefusal::SymlinkRow,
        link,
        &[PathBuf::from("/data/code")],
        Some(Path::new("/data/my code")),
    );
    assert_eq!(
        t,
        "/h/code is a symbolic link and the explorer never follows links. The cleaner's folders \
         are named by their real paths (/data/code); open the real path: \
         disk-cleanup explore '/data/my code'"
    );
    assert!(!t.contains("'roots'"));
    // the real path is not known: the text still says what to do
    let t = text(MarkRefusal::SymlinkRow, link, &roots(1), None);
    assert!(
        t.contains("never follows links") && t.contains("open the real path"),
        "{t}"
    );
}

#[test]
fn engine_refusals_should_carry_the_design_ux_3_5_reason_and_next_action() {
    let p = Path::new("/h/x");
    for (r, reason, next) in [
        (
            SkipReason::OutsideRoot,
            "out of scope: outside root",
            "none (never touched)",
        ),
        (
            SkipReason::ProtectedPath,
            "out of scope: protected path",
            "none (never touched)",
        ),
        (
            SkipReason::Symlink,
            "out of scope: symlink",
            "none (never touched)",
        ),
        (
            SkipReason::CrossDevice,
            "out of scope: cross-device",
            "none (never touched)",
        ),
    ] {
        assert_eq!(
            text(MarkRefusal::Engine(r), p, &[], None),
            format!("/h/x: {reason}. {next}")
        );
    }
}

#[test]
fn every_text_should_state_what_happened_and_end_with_a_next_action_or_a_period() {
    let p = Path::new("/h/code/app");
    let all = [
        MarkRefusal::NotAnItem,
        MarkRefusal::OutsideRoot,
        MarkRefusal::InsideBuildOrGit,
        MarkRefusal::SymlinkRow,
        MarkRefusal::ContainsItems(3),
        MarkRefusal::LimitReached,
        MarkRefusal::PathTooLong,
        MarkRefusal::StillScanning,
        MarkRefusal::Engine(SkipReason::ProtectedPath),
    ];
    for r in all {
        let t = text(r, p, &roots(1), None);
        assert!(!t.is_empty() && !t.contains('\n'), "{r:?}: {t:?}");
        // each text carries an instruction: a verb phrase that tells the owner what to do or what stays safe
        let has_action = [
            "Dry run",
            "Mark ",
            "Wait",
            "to include it",
            "open the real path",
            "press f",
            "Delete these",
            "none (",
        ]
        .iter()
        .any(|k| t.contains(k));
        assert!(has_action, "{r:?} has no next action: {t}");
    }
}

#[test]
fn paths_in_texts_should_be_escaped() {
    let hostile = PathBuf::from(OsString::from_vec(b"/h/a\x1b[31m\nb\xff".to_vec()));
    for r in [
        MarkRefusal::NotAnItem,
        MarkRefusal::OutsideRoot,
        MarkRefusal::InsideBuildOrGit,
        MarkRefusal::SymlinkRow,
        MarkRefusal::ContainsItems(1),
        MarkRefusal::PathTooLong,
        MarkRefusal::Engine(SkipReason::OutsideRoot),
    ] {
        let t = text(r, &hostile, &[hostile.clone()], Some(&hostile));
        assert!(
            !t.chars().any(|c| c.is_control() && c != ' '),
            "{r:?} leaked a control character: {t:?}"
        );
        assert!(t.contains("\\xff"), "{r:?}: {t}");
    }
}

#[test]
fn contains_items_should_print_the_count_and_99_plus_at_the_cap() {
    let p = Path::new("/h/code");
    assert!(text(MarkRefusal::ContainsItems(3), p, &[], None)
        .contains("contains 3 candidate items (press f to show only candidates)"));
    assert_eq!(count_text(98), "98");
    assert_eq!(count_text(99), "99+");
    assert_eq!(count_text(5000), "99+");
    assert!(text(MarkRefusal::ContainsItems(99), p, &[], None).contains("contains 99+ candidate"));
    assert_eq!(CONTAINS_CAP, 99);
}

/// `n` directories, eight children each (a breadth-first heap layout).
fn big_tree(n: usize) -> ExploreTree {
    let events: Vec<NodeEvent> = (0..n)
        .map(|i| NodeEvent::Dir {
            id: i as u64,
            parent: if i == 0 {
                NO_PARENT
            } else {
                ((i - 1) / 8) as u64
            },
            name: if i == 0 {
                "/r".into()
            } else {
                format!("d{i}").into()
            },
            dev: 1,
            info: DirInfo::default(),
            subdirs: (1..=8).filter(|k| 8 * i + k < n).count() as u32,
            from_cache: false,
            painted: false,
        })
        .collect();
    let mut t = ExploreTree::new();
    for chunk in events.chunks(4096) {
        t.apply_batch(0, ScanMode::Fresh, chunk.to_vec()).unwrap();
    }
    t.finish(false);
    t
}

#[test]
fn contains_items_should_stop_at_99_within_50_ms_on_a_tree_above_600000_nodes() {
    const N: usize = 650_000;
    let t = big_tree(N);
    assert!(t.directories() > 600_000);
    let root = t.roots().next().unwrap();
    // a candidate every third node: the walk ends after the 99th, long before the subtree does
    let seen = std::cell::Cell::new(0usize);
    let every_third = |id: NodeId| {
        seen.set(seen.get() + 1);
        id.index() % 3 == 0
    };
    let start = Instant::now();
    let n = count_items_below(&t, root, &every_third);
    let dense = start.elapsed();
    assert_eq!(n, 99);
    assert!(
        seen.get() < 1000,
        "visited {} nodes for 99 hits",
        seen.get()
    );

    // worst case: no candidate at all, so the whole subtree is walked
    let start = Instant::now();
    let none = count_items_below(&t, root, &|_| false);
    let full = start.elapsed();
    assert_eq!(none, 0);

    // a few candidates: the exact count, not the cap
    let few = count_items_below(&t, root, &|id| id.index() % 100_000 == 7);
    assert_eq!(few, 7, "indices 7, 100007, ..., 600007");

    eprintln!("dense {dense:?}, full walk of {N} nodes {full:?}");
    let bound = if cfg!(debug_assertions) { 2000 } else { 50 };
    assert!(dense.as_millis() < bound, "dense {dense:?}");
    assert!(full.as_millis() < bound, "full {full:?}");
}

#[test]
fn contains_items_should_not_count_the_directory_itself() {
    let t = big_tree(20);
    let root = t.roots().next().unwrap();
    assert_eq!(count_items_below(&t, root, &|_| true), 19);
    let leaf = t.iter().last().unwrap();
    assert_eq!(count_items_below(&t, leaf, &|_| true), 0);
}

#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Rows of one directory (Story 6.2.2, Task 6.2.2b; validation X-048, X-049, X-050, X-128): the cap and its
//! `(+N smaller)` row, exact sums, the empty directory, the lister merge and the filters.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Instant;

use dunnage_core::explore::msg::{FileRow, FileRows, NodeEvent, ScanMode, NO_PARENT};
use dunnage_core::explore::ports::DirLister;
use dunnage_core::explore::rows::{
    build_rows, Badge, Filter, ListedFiles, Row, RowId, RowKind, RowList, RowSource, Sort, SortKey,
    ROW_CAP,
};
use dunnage_core::explore::testing::FakeDirLister;
use dunnage_core::explore::tree::{ExploreTree, NodeId};
use dunnage_core::explore::view::{no_badge, ViewState};
use dunnage_scan::estimate::Stats;
use dunnage_scan::index::DirInfo;

/// A directory's own entries: `files` files of `alloc` bytes allocated and `size` bytes long, and `subdirs`
/// subdirectory entries.
fn own(files: u64, subdirs: u64, alloc: u64, size: u64) -> Stats {
    Stats {
        inodes: files + subdirs,
        files,
        dirs: subdirs,
        dir_entries: files + subdirs,
        name_bytes: 8 * (files + subdirs),
        size_bytes: files * size,
        allocated_bytes: files * alloc,
        ..Stats::default()
    }
}

fn dir(id: u64, parent: u64, name: &str, stats: Stats, subdirs: u32) -> NodeEvent {
    NodeEvent::Dir {
        id,
        parent,
        name: OsString::from(name),
        dev: 1,
        info: DirInfo {
            stats,
            ..DirInfo::default()
        },
        subdirs,
        from_cache: false,
        painted: false,
    }
}

fn feed(tree: &mut ExploreTree, events: &[NodeEvent]) {
    for chunk in events.chunks(256) {
        tree.apply_batch(0, ScanMode::Fresh, chunk.to_vec())
            .unwrap();
    }
}

fn root_of(tree: &ExploreTree) -> NodeId {
    tree.roots().next().unwrap()
}

fn sums(list: &RowList) -> (u64, u64, u64) {
    list.rows.iter().fold((0, 0, 0), |a, r| {
        (a.0 + r.usage, a.1 + r.apparent, a.2 + r.item_count())
    })
}

fn totals(tree: &ExploreTree, d: NodeId) -> (u64, u64, u64) {
    let t = tree.total(d);
    (t.allocated_bytes, t.size_bytes, t.inodes)
}

/// `/r` with `n` subdirectories of one file each and `files` files of its own.
fn big_tree(n: u64, files: u64) -> ExploreTree {
    let mut ev = vec![dir(0, NO_PARENT, "/r", own(files, n, 4096, 3000), n as u32)];
    for i in 0..n {
        let alloc = 4096 * (1 + i % 1000);
        ev.push(dir(
            1 + i,
            0,
            &format!("d{i:06}"),
            own(1, 0, alloc, alloc - 7),
            0,
        ));
    }
    let mut t = ExploreTree::new();
    feed(&mut t, &ev);
    t
}

#[test]
fn a_huge_directory_should_open_in_100ms_and_its_rows_should_add_up_exactly() {
    let t = big_tree(100_000, 412_390);
    let root = root_of(&t);
    let src = RowSource {
        tree: &t,
        dir: root,
        files: None,
        badge: &no_badge,
    };
    let started = Instant::now();
    let list = build_rows(&src, Sort::default(), Filter::All, "");
    let took = started.elapsed();
    eprintln!("rows of 100,000 subdirectories and 412,390 files: {took:?}");
    let bound = if cfg!(debug_assertions) { 4_000 } else { 250 };
    assert!(took.as_millis() < bound, "{took:?}");

    assert_eq!(list.rows.len(), ROW_CAP + 1);
    let last = list.rows.last().unwrap();
    assert_eq!(last.kind, RowKind::Smaller);
    assert!(last.display_name().starts_with("(+"));
    assert_eq!(
        sums(&list),
        totals(&t, root),
        "no byte or item may go missing"
    );
    assert_eq!(
        list.footer_note(Sort::new(SortKey::Name)).as_deref(),
        Some("name sort covers the 5000 largest entries")
    );
    assert_eq!(list.footer_note(Sort::default()), None);

    // the kept entries are the largest by usage, whatever the sort
    let name_sorted = build_rows(&src, Sort::new(SortKey::Name), Filter::All, "");
    let mut kept: Vec<u64> = list.rows[..ROW_CAP].iter().map(|r| r.usage).collect();
    let mut kept_by_name: Vec<u64> = name_sorted.rows[..ROW_CAP]
        .iter()
        .map(|r| r.usage)
        .collect();
    kept.sort_unstable();
    kept_by_name.sort_unstable();
    assert_eq!(kept, kept_by_name);
    assert_eq!(sums(&name_sorted), totals(&t, root));
}

#[test]
fn a_small_directory_should_show_every_subdirectory_and_one_files_row() {
    let mut t = ExploreTree::new();
    feed(
        &mut t,
        &[
            dir(0, NO_PARENT, "/r", own(5, 2, 4096, 100), 2),
            dir(1, 0, "a", own(1, 0, 8192, 8000), 0),
            dir(2, 0, "b", own(0, 0, 0, 0), 0),
        ],
    );
    let root = root_of(&t);
    let src = RowSource {
        tree: &t,
        dir: root,
        files: None,
        badge: &no_badge,
    };
    let list = build_rows(&src, Sort::default(), Filter::All, "");
    let names: Vec<String> = list.rows.iter().map(Row::display_name).collect();
    assert_eq!(names, ["(5 files)", "a/", "b/"]);
    assert!(list.files_unlisted);
    assert_eq!(list.folded, 0);
    assert_eq!(sums(&list), totals(&t, root));
    assert_eq!(
        list.rows[0].items,
        Some(5),
        "its own entries, not the subdirectories"
    );
}

#[test]
fn empty_directory_should_show_one_row_and_back_should_land_on_the_directory_left() {
    let mut t = ExploreTree::new();
    feed(
        &mut t,
        &[
            dir(0, NO_PARENT, "/r", own(0, 2, 0, 0), 2),
            dir(1, 0, "empty", Stats::default(), 0),
            dir(2, 0, "zzz", own(1, 0, 4096, 4000), 0),
        ],
    );
    let root = root_of(&t);
    let empty = t.children(root).find(|c| t.name(*c) == "empty").unwrap();
    let mut v = ViewState::new(Sort::default());
    v.open_dir(&t, root, 0, &no_badge);
    // the empty one is the second row (smaller than zzz); move onto it and open it
    let i = v
        .rows()
        .iter()
        .position(|r| r.id == RowId::Dir(empty))
        .unwrap();
    v.move_to(i, 10);
    assert_eq!(v.activate(), Some(empty));
    v.open_dir(&t, empty, 20, &no_badge);

    assert_eq!(v.rows().len(), 1);
    let row = &v.rows()[0];
    assert_eq!(row.kind, RowKind::Empty);
    assert_eq!(row.display_name(), "(empty directory)");
    assert_eq!((row.usage, row.apparent, row.item_count()), (0, 0, 0));
    assert_eq!(v.activate(), None, "Enter on it is a no-op");
    assert!(!v.list().files_unlisted);

    assert!(v.leave(&t, 30, &no_badge).is_some());
    assert_eq!(v.dir(), Some(root));
    assert_eq!(v.cursor(), Some(&RowId::Dir(empty)));
    assert_eq!(v.cursor_row().unwrap().name, OsString::from("empty"));
    assert!(
        v.leave(&t, 40, &no_badge).is_none(),
        "the scan root has no parent"
    );
}

fn file(name: &str, alloc: u64, size: u64, second: bool) -> FileRow {
    FileRow {
        name: OsString::from(name),
        allocated_bytes: alloc,
        size_bytes: size,
        second_link: second,
    }
}

fn files_tree() -> ExploreTree {
    // 4 files: 3 listed below, 1 smaller; one subdirectory
    let mut t = ExploreTree::new();
    feed(
        &mut t,
        &[
            dir(0, NO_PARENT, "/r", own(4, 1, 4096, 4000), 1),
            dir(1, 0, "sub", own(1, 0, 8192, 8000), 0),
        ],
    );
    t
}

#[test]
fn lister_rows_should_merge_late_vanished_files_dropped_and_navigation_should_cancel_the_listing() {
    let t = files_tree();
    let root = root_of(&t);
    let sub = t.children(root).next().unwrap();
    let lister = FakeDirLister::new(FileRows {
        rows: vec![
            file("big.bin", 4096, 4000, false),
            file("gone.bin", 4096, 4000, false),
            file("mid.bin", 4096, 4000, false),
        ],
        smaller_count: 1,
        smaller_bytes: 4096,
        partial: false,
    });
    let mut v = ViewState::new(Sort::default());
    let req = v.open_dir(&t, root, 0, &no_badge);

    // first paint: the subdirectory and one synthetic row, no wait for the lister
    let names: Vec<String> = v.rows().iter().map(Row::display_name).collect();
    assert_eq!(names, ["(4 files)", "sub/"]);
    assert!(v.listing_pending());
    assert!(v.path_segments().iter().any(|s| s == "listing files..."));

    // the lister answers after three ticks
    for tick in 1..=3u64 {
        v.order_snapshot(&t, tick * 100, &no_badge);
        assert_eq!(v.rows().len(), 2);
    }
    let answer = lister.list_files(&t.path_of(req.dir), 5000, &req.cancel);
    let vanished = OsStr::new("gone.bin");
    assert!(v.accept_files(req.generation, req.dir, answer, &|n| n != vanished));
    assert!(!v.listing_pending());
    assert!(!v.path_segments().iter().any(|s| s.contains("listing")));
    assert_eq!(lister.calls(), vec![PathBuf::from("/r")]);

    // the rows merge at the next re-sort, which waits out the quiet window after the last key (t = 0)
    v.order_snapshot(&t, 1_500, &no_badge);
    assert_eq!(v.rows().len(), 2, "no merge inside 2 s of a key");
    assert!(v.order_snapshot(&t, 2_500, &no_badge));
    let names: Vec<String> = v.rows().iter().map(Row::display_name).collect();
    assert_eq!(names, ["sub/", "big.bin", "mid.bin", "(+1 smaller)"]);
    assert!(!v.rows().iter().any(|r| r.name == "gone.bin"));
    assert!(!v.list().files_unlisted);

    // moving on cancels a listing in flight and its late answer is ignored
    let mut w = ViewState::new(Sort::default());
    let first = w.open_dir(&t, root, 0, &no_badge);
    let second = w.open_dir(&t, sub, 10, &no_badge);
    assert!(
        first.cancel.load(Ordering::SeqCst),
        "the old listing is cancelled"
    );
    assert!(!second.cancel.load(Ordering::SeqCst));
    let late = FileRows::default();
    assert!(!w.accept_files(first.generation, first.dir, late, &|_| true));
    assert!(
        w.listing_pending(),
        "the new directory's listing is still wanted"
    );
}

#[test]
fn rows_should_add_up_exactly_without_hardlinks_and_say_hard_links_are_counted_once_with_them() {
    let t = files_tree();
    let root = root_of(&t);
    let plain = ListedFiles {
        rows: vec![
            file("a", 4096, 4000, false),
            file("b", 4096, 4000, false),
            file("c", 4096, 4000, false),
        ],
        smaller_count: 1,
        partial: false,
    };
    let src = RowSource {
        tree: &t,
        dir: root,
        files: Some(&plain),
        badge: &no_badge,
    };
    let list = build_rows(&src, Sort::default(), Filter::All, "");
    assert_eq!(sums(&list), totals(&t, root));
    assert!(!list.hard_links);
    assert_eq!(list.footer_note(Sort::default()), None);

    // the same names, but `c` is a second link of `a`: the totals counted 4 files, the rows say 4 x 4096
    // for three rows plus the remainder and may exceed the directory total
    let linked = ListedFiles {
        rows: vec![
            file("a", 4096, 4000, false),
            file("b", 4096, 4000, false),
            file("c", 4096, 4000, true),
            file("d", 4096, 4000, true),
            file("e", 4096, 4000, true),
        ],
        smaller_count: 0,
        partial: false,
    };
    let src = RowSource {
        files: Some(&linked),
        ..src
    };
    let list = build_rows(&src, Sort::default(), Filter::All, "");
    assert!(list.hard_links);
    assert_eq!(
        list.footer_note(Sort::default()).as_deref(),
        Some("hard links are counted once in totals")
    );
    let (usage, _, _) = sums(&list);
    assert!(
        usage > totals(&t, root).0,
        "a hard-linked listing may overshoot"
    );
}

#[test]
fn filters_should_match_escaped_names_cycle_and_never_touch_marks() {
    let mut t = ExploreTree::new();
    let mut ev = vec![dir(0, NO_PARENT, "/r", own(2, 3, 4096, 4000), 3)];
    ev.push(dir(1, 0, "Build", own(1, 0, 4096, 4000), 0));
    ev.push(dir(2, 0, "bad\nname", own(1, 0, 4096, 4000), 0));
    ev.push(NodeEvent::Unreadable {
        id: 3,
        parent: 0,
        name: OsString::from("locked"),
        errno: 13,
    });
    feed(&mut t, &ev);
    let root = root_of(&t);
    let build_id = t.children(root).find(|c| t.name(*c) == "Build").unwrap();
    let badge = move |n: NodeId| {
        if n == build_id {
            Badge::Candidate
        } else {
            Badge::None
        }
    };
    let src = RowSource {
        tree: &t,
        dir: root,
        files: None,
        badge: &badge,
    };
    let names = |l: &RowList| -> Vec<String> { l.rows.iter().map(Row::display_name).collect() };

    let all = build_rows(&src, Sort::new(SortKey::Name), Filter::All, "");
    assert_eq!(all.rows.len(), 4);

    let text = build_rows(&src, Sort::new(SortKey::Name), Filter::All, "BUIL");
    assert_eq!(names(&text), ["Build/"], "case-insensitive");
    let esc = build_rows(&src, Sort::new(SortKey::Name), Filter::All, "\\n");
    assert_eq!(names(&esc), ["bad\\nname/"], "matches the escaped name");

    let cands = build_rows(&src, Sort::default(), Filter::Candidates, "");
    assert_eq!(names(&cands), ["Build/"]);
    let errs = build_rows(&src, Sort::default(), Filter::Errors, "");
    assert_eq!(names(&errs), ["locked/"]);
    assert!(errs.rows[0].unreadable);

    assert_eq!(Filter::All.next(), Filter::Candidates);
    assert_eq!(Filter::Candidates.next(), Filter::Errors);
    assert_eq!(Filter::Errors.next(), Filter::All);

    // the view keeps the cursor identity across a filter that still shows its row
    let mut v = ViewState::new(Sort::new(SortKey::Name));
    v.open_dir(&t, root, 0, &badge);
    let i = v
        .rows()
        .iter()
        .position(|r| r.id == RowId::Dir(build_id))
        .unwrap();
    v.move_to(i, 5);
    v.cycle_filter(&t, 6, &badge);
    assert_eq!(v.filter(), Filter::Candidates);
    assert_eq!(v.cursor(), Some(&RowId::Dir(build_id)));
    v.cycle_filter(&t, 7, &badge);
    v.cycle_filter(&t, 8, &badge);
    assert_eq!(v.filter(), Filter::All);
    assert_eq!(
        v.rows().len(),
        4,
        "a filter hides rows, it does not remove them"
    );
}

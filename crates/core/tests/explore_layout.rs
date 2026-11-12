#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Story 6.2.3 (Task 6.2.3b): tiers and columns, the too-small message, name cutting, the title, the
//! header and row lines, the vertical budget and the headroom forms (UX-T13, UX-T14, UX-T15, UX-T17, G-7).

use std::path::{Path, PathBuf};

use dunnage_core::explore::fmt::{fmt_items_cell, ITEMS_CELL_MAX};
use dunnage_core::explore::headroom::headroom_line;
use dunnage_core::explore::layout::{
    cells, cut_name, draw_row, footer_path_line, header_line, layout_for_width, path_line,
    split_atoms, title_left, title_line, too_small_lines, vertical_budget, ColumnKind, Layout,
    RowCells, SortMark, Tier, TierLayout,
};
use dunnage_core::{AllocSnapshot, FsSnapshot, SpaceUse};

const GIB: u64 = 1 << 30;

fn tiered(w: usize, h: usize) -> TierLayout {
    match layout_for_width(w, h) {
        Layout::Tiered(l) => l,
        Layout::TooSmall { .. } => panic!("{w}x{h} should be tiered"),
    }
}

fn kinds(l: &TierLayout) -> Vec<ColumnKind> {
    l.columns.iter().map(|c| c.kind).collect()
}

#[test]
fn the_tiers_should_follow_the_table_at_120_100_99_80_79_and_60_columns() {
    use ColumnKind::{Apparent, Items, Meta, Share, Usage};
    let five = vec![Usage, Apparent, Meta, Items, Share];
    let cases: [(usize, Tier, Vec<ColumnKind>, usize, usize, usize); 6] = [
        (120, Tier::Wide, five.clone(), 10, 65, 55),
        (100, Tier::Wide, five.clone(), 10, 65, 35),
        (99, Tier::Compact, five.clone(), 5, 55, 44),
        (80, Tier::Compact, five, 5, 55, 25),
        (79, Tier::Narrow, vec![Usage, Items, Share], 5, 35, 44),
        (60, Tier::Narrow, vec![Usage, Items, Share], 5, 35, 25),
    ];
    for (w, tier, cols, bar, fixed, name) in cases {
        let l = tiered(w, 24);
        assert_eq!(l.tier, tier, "w={w}");
        assert_eq!(kinds(&l), cols, "w={w}");
        assert_eq!(
            (l.bar_cells, l.fixed, l.name_width),
            (bar, fixed, name),
            "w={w}"
        );
        let share = l.columns.last().unwrap().width;
        assert_eq!(share, if bar == 10 { 17 } else { 12 });
    }
}

#[test]
fn below_60_columns_or_12_rows_or_at_0x0_only_the_three_line_message_should_be_drawn() {
    for (w, h) in [(59, 24), (60, 11), (0, 0), (1, 1), (40, 10), (200, 5)] {
        assert!(
            matches!(layout_for_width(w, h), Layout::TooSmall { .. }),
            "{w}x{h}"
        );
    }
    assert!(matches!(layout_for_width(60, 12), Layout::Tiered(_)));
    let msg = too_small_lines(40, 10);
    assert_eq!(msg[0].trim(), "Too small: need 60x12, have 40x10.");
    assert_eq!(msg.len(), 3);
    assert!(msg.iter().all(|l| l.trim().chars().count() <= 36));
}

#[test]
fn the_too_small_message_should_render_for_every_width_without_panic_and_fit_its_screen() {
    for w in 0..=59 {
        for h in [0, 1, 2, 3, 10, 11, 100, 100_000] {
            let lines = too_small_lines(w, h);
            assert_eq!(lines.len(), h.min(3));
            assert!(lines
                .iter()
                .all(|l| cells(l) <= w && l.trim().chars().count() <= 36));
        }
    }
    let wide_short = too_small_lines(100_000, 11);
    assert!(
        wide_short.iter().all(|l| l.trim().chars().count() <= 36),
        "{wide_short:?}"
    );
}

#[test]
fn the_vertical_budget_should_give_17_list_rows_at_24_and_5_at_12() {
    let v = vertical_budget(24);
    assert_eq!(
        (v.list_rows, v.info_lines, v.info_replaces_list),
        (17, 8, false)
    );
    assert_eq!(v.list_rows_with_info, 9);
    let v = vertical_budget(12);
    assert_eq!(v.list_rows, 5);
    assert!(v.info_replaces_list);
    assert!(!vertical_budget(20).info_replaces_list);
    assert!(vertical_budget(19).info_replaces_list);
    assert_eq!(tiered(120, 24).vertical.list_rows, 17);
}

#[test]
fn the_name_should_end_in_dots_and_never_be_cut_inside_an_escape_or_a_wide_character() {
    assert_eq!(cut_name("short/", 25), "short/");
    let long = "a".repeat(300);
    let cut = cut_name(&long, 25);
    assert_eq!((cut.len(), cut.ends_with("...")), (25, true));
    // The cut lands inside `\u{1f389}`: the whole escape is dropped.
    let esc = "abc\\u{1f389}def";
    assert_eq!(cut_name(esc, 9), "abc...");
    assert_eq!(
        cut_name(esc, 12),
        "abc\\u{1f389}...".replace("abc\\u{1f389}...", "abc...")
    );
    // A wide character is kept whole or dropped, never split: width 2 each.
    let cjk = "\u{65e5}\u{672c}\u{8a9e}\u{6587}\u{5b57}";
    assert_eq!(cut_name(cjk, 9), "\u{65e5}\u{672c}\u{8a9e}...");
    assert_eq!(cut_name(cjk, 8), "\u{65e5}\u{672c}...");
    assert_eq!(cells(&cut_name(cjk, 8)), 7);
    assert_eq!(cut_name("abcdef", 3), "...");
    assert_eq!(cut_name("abcdef", 2), "..");
    assert_eq!(split_atoms("a\\x1b\\\\b"), ["a", "\\x1b", "\\\\", "b"]);
}

#[test]
fn the_title_should_use_its_long_right_hand_text_only_when_left_plus_right_plus_2_fits() {
    let left = title_left(120, None);
    let (long, short) = (
        "SCAN DONE 6.2s (cache warm, 41,288 dirs, 0 unreadable)",
        "SCAN DONE 6.2s",
    );
    let need = cells(&left) + cells(long) + 2;
    assert_eq!((cells(&left), cells(long)), (47, 54));
    let at = title_line(&left, long, short, need);
    assert!(at.ends_with(long) && cells(&at) == need);
    let below = title_line(&left, long, short, need - 1);
    assert!(below.ends_with(short) && cells(&below) == need - 1);
    assert_eq!(cells(&title_line(&left, long, short, 120)), 120);
    assert_eq!(title_left(79, None), "EXPLORE (d deletes)");
    assert_eq!(title_left(120, Some((2, 3))), "DELETING 2 of 3");
    assert!(cells(&title_line(&left, long, short, 60)) <= 60);
}

#[test]
fn the_path_line_should_drop_trailing_segments_before_cutting_the_location() {
    let segs = [
        "224.3 GiB (247.0 GiB apparent)",
        "sort: usage, largest first",
        "filter: none",
        "marks: 0",
    ];
    let full = path_line("~", &segs, 120);
    assert_eq!(
        full,
        "~   224.3 GiB (247.0 GiB apparent)   sort: usage, largest first   filter: none   marks: 0"
    );
    let tight = path_line("~", &segs, 70);
    assert!(cells(&tight) <= 70 && tight.contains("sort: usage") && !tight.contains("filter"));
    assert!(cells(&path_line(&"x".repeat(200), &segs, 60)) <= 60);
}

#[test]
fn the_footer_path_should_be_cut_from_the_left_with_a_dots_prefix() {
    assert_eq!(
        footer_path_line("/home/t/.cache/", 80),
        "Path: /home/t/.cache/"
    );
    let long = format!("/{}/leaf\\x1b", "d".repeat(100));
    let line = footer_path_line(&long, 60);
    assert!(cells(&line) <= 60 && line.starts_with("Path: ...") && line.ends_with("leaf\\x1b"));
}

#[test]
fn the_header_should_align_labels_and_mark_the_sort_column() {
    let sort = Some(SortMark {
        column: ColumnKind::Usage,
        descending: true,
    });
    assert_eq!(
        header_line(&tiered(120, 24), sort),
        "       USAGEv   APPARENT       META    ITEMS  SHARE              NAME"
    );
    assert_eq!(
        header_line(&tiered(80, 24), sort),
        "       USAGEv  APPARENT      META   ITEMS SHARE        NAME"
    );
    assert_eq!(
        header_line(&tiered(60, 24), sort),
        "       USAGEv   ITEMS SHARE        NAME"
    );
    let asc = Some(SortMark {
        column: ColumnKind::Name,
        descending: false,
    });
    assert!(header_line(&tiered(80, 24), asc).ends_with("NAME^"));
    assert!(header_line(&tiered(80, 24), None).ends_with("NAME"));
}

fn row(name: &str) -> RowCells {
    RowCells {
        cursor: true,
        marked: true,
        state: '+',
        usage: 217 * GIB / 10,
        apparent: 249 * GIB / 10,
        meta: Some(610 * (1 << 20)),
        items: Some(412_330),
        parent_usage: 336 * GIB / 10,
        name: name.to_string(),
    }
}

#[test]
fn rows_should_match_the_wireframes_of_the_wide_compact_and_narrow_tiers() {
    assert_eq!(
        draw_row(&tiered(80, 24), &row("node_modules/")),
        ">*+  21.7 GiB  24.9 GiB 610.0 MiB  412330 [###..]  65% node_modules/"
    );
    let mut r = row("code/");
    (r.cursor, r.marked, r.state) = (false, false, ' ');
    (r.usage, r.apparent, r.meta, r.items, r.parent_usage) = (
        1124 * GIB / 10,
        131 * GIB,
        Some(19 * GIB / 10),
        Some(1_203_455),
        2243 * GIB / 10,
    );
    assert_eq!(
        draw_row(&tiered(120, 24), &r),
        "    112.4 GiB  131.0 GiB    1.9 GiB  1203455  [#####.....]  50%  code/"
    );
    let mut n = row("home/");
    (n.cursor, n.marked, n.state, n.items) = (false, false, ' ', Some(2_810_334));
    n.usage = 1_500_000_000_000 * 93 / 100;
    n.parent_usage = 1_500_000_000_000;
    assert_eq!(
        draw_row(&tiered(60, 24), &n),
        "      1.3 TiB 2810334 [#####]  93% home/"
    );
    let mut file = row("package-lock.json");
    (file.meta, file.items) = (None, None);
    let line = draw_row(&tiered(80, 24), &file);
    assert!(line.contains("      n/a     n/a "), "{line}");
}

#[test]
fn item_counts_should_never_outgrow_their_7_character_column() {
    for (n, want) in [
        (0, "0"),
        (9_999_999, "9999999"),
        (10_000_000, "10.0M"),
        (123_456_789, "123.5M"),
        (999_999_999, "1.0G"),
        (u64::MAX, "18.4E"),
    ] {
        assert_eq!(fmt_items_cell(n), want, "{n}");
        assert!(fmt_items_cell(n).len() <= ITEMS_CELL_MAX);
    }
}

fn snapshot(fs_type: &str, btrfs: Option<AllocSnapshot>) -> FsSnapshot {
    FsSnapshot {
        path: PathBuf::from("/"),
        taken_at: 0,
        total_bytes: 1_800 * GIB,
        used_bytes: 1_728 * GIB,
        avail_bytes: 702 * GIB / 10,
        fs_type: fs_type.to_string(),
        btrfs,
    }
}

fn btrfs_sample(unalloc: u64) -> FsSnapshot {
    let alloc = AllocSnapshot {
        data: SpaceUse {
            bytes_used: 400 * GIB,
            total_bytes: 400 * GIB + 512 * GIB / 10,
        },
        metadata: SpaceUse {
            bytes_used: 20 * GIB,
            total_bytes: 21 * GIB + 9 * GIB / 10,
        },
        system: SpaceUse {
            bytes_used: 1 << 20,
            total_bytes: 32 << 20,
        },
        unallocated: unalloc,
    };
    snapshot("btrfs", Some(alloc))
}

#[test]
fn the_headroom_line_should_take_the_wide_compact_or_narrow_form_of_91_76_and_39_columns() {
    let s = btrfs_sample(98 * GIB / 10);
    let p = Path::new("/");
    let wide = headroom_line(p, Ok(&s), 120);
    assert_eq!(wide, "/ btrfs  avail 70.2 GiB  unallocated 9.8 GiB  data slack 51.2 GiB  meta slack 1.9 GiB  WARN");
    assert_eq!(cells(&wide), 91);
    let compact = headroom_line(p, Ok(&s), 90);
    assert_eq!(
        compact,
        "/ btrfs  avail 70.2 GiB  unalloc 9.8 GiB  slack data 51.2 meta 1.9 GiB  WARN"
    );
    assert_eq!(cells(&compact), 76);
    let narrow = headroom_line(p, Ok(&s), 75);
    assert_eq!(narrow, "/ avail 70.2 GiB  unalloc 9.8 GiB  WARN");
    assert_eq!(cells(&narrow), 39);
    assert_eq!(headroom_line(p, Ok(&s), 91), wide);
    assert_eq!(headroom_line(p, Ok(&s), 76), compact);
    assert_eq!(headroom_line(p, Ok(&s), 39), narrow);
}

#[test]
fn the_verdict_word_should_come_from_the_metascope_assessment() {
    let p = Path::new("/");
    assert!(
        headroom_line(p, Ok(&btrfs_sample(40 * GIB)), 120).ends_with("meta slack 1.9 GiB  WARN")
    );
    assert!(headroom_line(p, Ok(&btrfs_sample(2 * GIB)), 120).ends_with("  CRIT"));
    let mut healthy = btrfs_sample(40 * GIB);
    healthy.btrfs.as_mut().unwrap().metadata.total_bytes = 30 * GIB;
    assert!(headroom_line(p, Ok(&healthy), 120).ends_with("  OK"));
}

#[test]
fn other_filesystems_and_an_unreadable_state_should_use_their_own_forms() {
    let p = Path::new("/");
    let ext4 = snapshot("ext4", None);
    assert_eq!(
        headroom_line(p, Ok(&ext4), 120),
        "/ ext4  avail 70.2 GiB of 1.8 TiB  used 96%  OK"
    );
    assert_eq!(
        headroom_line(p, Err("permission denied"), 120),
        "/ filesystem state unreadable (permission denied)  unknown"
    );
}

#[test]
fn a_headroom_line_should_fit_every_width_and_keep_its_verdict_word() {
    let s = btrfs_sample(98 * GIB / 10);
    let long = Path::new("/mnt/a/very/long/mount/point/\u{65e5}\u{672c}\u{8a9e}/with\u{1b}esc");
    for w in 40..=200 {
        let line = headroom_line(long, Ok(&s), w);
        assert!(cells(&line) <= w, "w={w}: {line}");
        assert!(!line.contains('\u{1b}'));
        assert!(line.ends_with("WARN"), "w={w}: {line}");
    }
}

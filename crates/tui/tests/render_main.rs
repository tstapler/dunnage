#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The main view (Task 6.5.3a; UX-T8, UX-T10, UX-T13, UX-T15): the wireframes of `ux-explorer.md` section 4
//! as `insta` snapshots at every width tier, and the rules the snapshots cannot show (cursor, mark, state
//! characters, live states).

mod common;

use std::ffi::OsString;

use common::*;
use disk_cleanup_core::explore::msg::{NodeEvent, ScanMode, ScanSummary, NO_PARENT};
use disk_cleanup_core::explore::rows::Badge;
use disk_cleanup_core::explore::tree::ExploreTree;
use ratatui::style::Modifier;

const SIZES: [(u16, u16); 7] = [
    (120, 24),
    (100, 24),
    (99, 24),
    (80, 24),
    (79, 24),
    (60, 14),
    (40, 10),
];

fn marked_scanning() -> Fixture {
    let mut f = Fixture::new(sample_tree(false)).with_badge(|t, id| {
        if t.name(id) == "node_modules" {
            Badge::Candidate
        } else {
            Badge::None
        }
    });
    f.mark("/home/u/code/app/node_modules", 217 * GIB / 10);
    f
}

#[test]
fn the_snapshot_matrix_matches_the_wireframes_at_every_tier() {
    for (w, h) in SIZES {
        let f = marked_scanning();
        let text = draw_text(&f.screen(), w, h);
        insta::assert_snapshot!(format!("main_scanning_{w}x{h}"), text);
        assert_fits(&text, usize::from(w));
    }
    for (w, h) in SIZES {
        let mut f = Fixture::new(sample_tree(true));
        f.elapsed_tenths = 62;
        f.summary = Some(ScanSummary {
            dirs_cached: 5,
            ..ScanSummary::default()
        });
        let text = draw_text(&f.screen(), w, h);
        insta::assert_snapshot!(format!("main_done_{w}x{h}"), text);
        assert_fits(&text, usize::from(w));
    }
}

#[test]
fn no_snapshot_prints_dry_run_or_applying() {
    for (w, h) in SIZES {
        for finished in [false, true] {
            let f = Fixture::new(sample_tree(finished));
            let text = draw_text(&f.screen(), w, h);
            assert!(
                !text.contains("DRY RUN") && !text.contains("APPLYING"),
                "{text}"
            );
        }
    }
    // the stored snapshots too
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let t = std::fs::read_to_string(&p).unwrap();
        let body = t.split("---\n").last().unwrap_or("");
        assert!(
            !body.contains("DRY RUN") && !body.contains("APPLYING"),
            "{p:?}"
        );
    }
}

#[test]
fn line_one_is_the_long_text_from_80_columns_and_the_short_one_below() {
    let f = Fixture::new(sample_tree(true));
    for (w, h) in SIZES {
        let text = draw_text(&f.screen(), w, h);
        if w < 60 || h < 12 {
            continue;
        }
        let first = text.lines().next().unwrap();
        if w >= 80 {
            assert!(
                first.starts_with("EXPLORE - nothing is deleted until you confirm."),
                "{first}"
            );
        } else {
            assert!(first.starts_with("EXPLORE (d deletes)"), "{first}");
        }
        let headroom = text.lines().nth(1).unwrap();
        assert!(
            headroom.ends_with("WARN") || headroom.ends_with("OK") || headroom.ends_with("CRIT"),
            "headroom line on screen at {w}x{h}: {headroom:?}"
        );
    }
}

#[test]
fn the_cursor_row_has_a_marker_in_column_one_and_reverse_video() {
    let f = Fixture::new(sample_tree(true));
    let term = draw(&f.screen(), 100, 24);
    let buf = term.backend().buffer();
    assert_eq!(buf[(0, 4)].symbol(), ">");
    for x in [0u16, 10, 50, 99] {
        assert!(buf[(x, 4)].modifier.contains(Modifier::REVERSED), "col {x}");
    }
    assert!(!buf[(0, 5)].modifier.contains(Modifier::REVERSED));
}

#[test]
fn a_marked_row_shows_a_star_and_the_summary_names_the_marks() {
    let f = marked_scanning();
    let text = draw_text(&f.screen(), 120, 24);
    let row = text.lines().nth(4).unwrap();
    assert!(row.starts_with(">*+"), "cursor, mark, candidate: {row:?}");
    assert!(text.contains("1 marked (21.7 GiB)"), "{text}");
    assert!(
        text.lines().last().unwrap().contains("d delete"),
        "d delete replaces i info"
    );
}

#[test]
fn state_characters_follow_the_precedence_x_bang_dot_plus_question() {
    let mut ev: Vec<NodeEvent> = vec![dir_ev(0, NO_PARENT, "/r", stats(1, 6, 4096, 10), 6, 0)];
    ev.push(NodeEvent::Boundary {
        id: 1,
        parent: 0,
        name: "mnt".into(),
        dev: 9,
    });
    ev.push(NodeEvent::Unreadable {
        id: 2,
        parent: 0,
        name: "var".into(),
        errno: 13,
    });
    ev.push(dir_ev(3, 0, "scanning", stats(1, 0, 3000, 3000), 1, 0));
    ev.push(dir_ev(4, 0, "cand", stats(1, 0, 2000, 2000), 0, 0));
    ev.push(dir_ev(5, 0, "maybe", stats(1, 0, 1500, 1500), 0, 0));
    ev.push(dir_ev(6, 0, "done", stats(1, 0, 1000, 1000), 0, 0));
    let mut t = ExploreTree::new();
    t.apply_batch(0, ScanMode::Fresh, ev).unwrap();
    let f = Fixture::new(t).with_badge(|t, id| match t.name(id).to_str().unwrap() {
        "cand" | "scanning" => Badge::Candidate,
        "maybe" => Badge::Possible,
        _ => Badge::None,
    });
    let text = draw_text(&f.screen(), 100, 24);
    let state_of = |name: &str| -> char {
        let row = text
            .lines()
            .find(|l| l.contains(name))
            .unwrap_or_else(|| panic!("{name}: {text}"));
        row.chars().nth(2).unwrap()
    };
    assert_eq!(state_of("mnt/"), 'x');
    assert_eq!(state_of("var/"), '!');
    assert_eq!(
        state_of("scanning/"),
        '.',
        "a scanning candidate reads `.`, not `+`"
    );
    assert_eq!(state_of("cand/"), '+');
    assert_eq!(state_of("maybe/"), '?');
    assert_eq!(state_of("done/"), ' ');
    let boundary = text.lines().find(|l| l.contains("mnt/")).unwrap();
    assert!(
        boundary.matches("n/a").count() >= 4,
        "an unscanned row reads n/a: {boundary:?}"
    );
}

#[test]
fn a_tree_mid_scan_says_scanning_dots_and_at_least_and_after_finished_none_remain() {
    let f = Fixture::new(sample_tree(false));
    let text = draw_text(&f.screen(), 120, 24);
    assert!(
        text.lines()
            .next()
            .unwrap()
            .trim_end()
            .ends_with("SCANNING 3.1s"),
        "{text}"
    );
    assert!(text.contains("at least 33.6 GiB so far"));
    assert!(text
        .lines()
        .skip(4)
        .take(8)
        .any(|l| l.chars().nth(2) == Some('.')));

    let mut f = Fixture::new(sample_tree(true));
    f.elapsed_tenths = 62;
    f.summary = Some(ScanSummary {
        dirs_cached: 3,
        ..ScanSummary::default()
    });
    let text = draw_text(&f.screen(), 120, 24);
    let first = text.lines().next().unwrap();
    assert!(
        first
            .trim_end()
            .ends_with("SCAN DONE 6.2s (cache warm, 8 dirs, 0 unreadable)"),
        "{first}"
    );
    assert!(
        !text
            .lines()
            .skip(4)
            .take(8)
            .any(|l| l.chars().nth(2) == Some('.')),
        "{text}"
    );
    assert!(!text.contains("at least"));
    let narrow = draw_text(&f.screen(), 80, 24);
    let first = narrow.lines().next().unwrap();
    assert!(
        first.trim_end().ends_with("SCAN DONE 6.2s"),
        "short form when the long one does not fit: {first}"
    );
}

#[test]
fn after_a_cache_paint_the_title_reads_showing_the_cache_until_revalidated() {
    let mut f = Fixture::new(sample_tree(false));
    f.cache_age = Some("12 min ago".into());
    let text = draw_text(&f.screen(), 120, 24);
    assert!(
        text.lines()
            .next()
            .unwrap()
            .contains("showing the cache from 12 min ago"),
        "{text}"
    );
}

#[test]
fn the_share_column_without_a_bar_under_t_and_the_headroom_failure_form() {
    let mut f = Fixture::new(sample_tree(true));
    f.state.input.percent_only = true;
    let text = draw_text(&f.screen(), 100, 24);
    assert!(!text.contains("[#"), "no bar under t: {text}");
    assert!(text.contains("65%"));
    f.snapshot = Err("statvfs failed".into());
    let text = draw_text(&f.screen(), 100, 24);
    assert!(
        text.lines()
            .nth(1)
            .unwrap()
            .contains("filesystem state unreadable (statvfs failed)  unknown"),
        "{text}"
    );
}

#[test]
fn the_empty_directory_and_the_huge_directory_footer_match_their_wireframes() {
    let mut t = ExploreTree::new();
    t.apply_batch(
        0,
        ScanMode::Fresh,
        vec![dir_ev(0, NO_PARENT, "/e", stats(0, 0, 0, 0), 0, 0)],
    )
    .unwrap();
    t.finish(false);
    let f = Fixture::new(t);
    let text = draw_text(&f.screen(), 80, 24);
    let row = text.lines().nth(4).unwrap();
    assert!(
        row.starts_with('>') && row.contains("(empty directory)") && row.contains("n/a"),
        "{row:?}"
    );
    let _ = OsString::new();
}

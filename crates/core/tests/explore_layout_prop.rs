#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Story 6.2.3 (Task 6.2.3c): no drawn row, header or title line is wider than the terminal for any width
//! from 60 to 200, with hostile and wide names (UX-T13, UX-34), and a name is never cut inside an escape.

use dunnage_core::escape::escape;
use dunnage_core::explore::layout::{
    cells, cut_name, draw_row, footer_path_line, header_line, layout_for_width, split_atoms,
    title_left, title_line, ColumnKind, Layout, RowCells, SortMark,
};
use proptest::prelude::*;

fn name_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        "[a-zA-Z0-9._-]{0,40}".prop_map(String::into_bytes),
        "[\u{4e00}-\u{9fff}]{0,60}".prop_map(String::into_bytes),
        "[\u{1f300}-\u{1f64f}]{0,40}".prop_map(String::into_bytes),
        "[a-z]{300}".prop_map(String::into_bytes),
        proptest::collection::vec(any::<u8>(), 0..80),
        "\\PC{0,50}".prop_map(String::into_bytes),
        ".{0,50}".prop_map(String::into_bytes),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn every_drawn_line_should_fit_every_width_from_60_to_200(
        raw in name_strategy(),
        usage in any::<u64>(), apparent in any::<u64>(), parent in any::<u64>(),
        meta in proptest::option::of(any::<u64>()), items in proptest::option::of(any::<u64>()),
        tty in any::<bool>(),
    ) {
        let mut name = escape(&raw, tty);
        name.push('/');
        let row = RowCells {
            cursor: true, marked: true, state: '+', usage, apparent, meta, items,
            parent_usage: parent, name: name.clone(),
        };
        for w in 60..=200 {
            let Layout::Tiered(l) = layout_for_width(w, 24) else { panic!("{w}") };
            let line = draw_row(&l, &row);
            prop_assert!(cells(&line) <= w, "w={w} {line:?}");
            prop_assert!(!line.contains('\x1b') && !line.contains('\n'));
            let hdr = header_line(&l, Some(SortMark { column: ColumnKind::Usage, descending: true }));
            prop_assert!(cells(&hdr) <= w);
            let title = title_line(&title_left(w, None), "SCAN DONE 6.2s (cache warm, 41,288 dirs, 0 unreadable)", "SCAN DONE 6.2s", w);
            prop_assert!(cells(&title) <= w, "{title:?}");
            prop_assert!(cells(&footer_path_line(&name, w)) <= w);
        }
    }

    #[test]
    fn a_cut_name_should_be_whole_atoms_ending_in_dots_when_shortened(
        raw in name_strategy(), width in 4usize..120,
    ) {
        let name = escape(&raw, true);
        let cut = cut_name(&name, width);
        prop_assert!(cells(&cut) <= width);
        if cells(&name) <= width {
            prop_assert_eq!(&cut, &name);
        } else {
            prop_assert!(cut.ends_with("..."));
            let body = &cut[..cut.len() - 3];
            prop_assert!(name.starts_with(body));
            // The cut is on an atom boundary of the original: no escape and no character is split.
            let mut at = 0;
            let mut boundary = body.is_empty();
            for a in split_atoms(&name) {
                at += a.len();
                if at == body.len() { boundary = true; }
            }
            prop_assert!(boundary, "cut inside an atom: {body:?} of {name:?}");
        }
    }
}

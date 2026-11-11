#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Story 6.2.3 (Task 6.2.3a): the single escape rule, the size cell and the share cell.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use disk_cleanup_core::escape::{escape, needs_escape, unescape, FORMAT_RANGES};
use disk_cleanup_core::explore::fmt::{fmt_size_cell, share_cell, SIZE_CELL_MAX};
use proptest::prelude::*;

const GIB: u64 = 1 << 30;

/// The `FORMAT` list of `tui/tools/quote_ref.py`, generated there by a Python `unicodedata` loop over the
/// categories Cf, Zl and Zp of Unicode 16.0.0.
const GOLDEN_FORMAT: [(u32, u32); 21] = [
    (0xAD, 0xAD),
    (0x600, 0x605),
    (0x61C, 0x61C),
    (0x6DD, 0x6DD),
    (0x70F, 0x70F),
    (0x890, 0x891),
    (0x8E2, 0x8E2),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x2028, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
];

#[test]
fn the_format_table_should_equal_the_frozen_golden_list_of_21_ranges() {
    assert_eq!(FORMAT_RANGES, &GOLDEN_FORMAT[..]);
}

#[test]
fn needs_escape_should_hold_for_c0_del_c1_and_every_golden_format_range_only() {
    for c in 0u32..=0x10FFFF {
        let Some(ch) = char::from_u32(c) else {
            continue;
        };
        let golden = c < 0x20
            || (0x7F..=0x9F).contains(&c)
            || GOLDEN_FORMAT.iter().any(|&(lo, hi)| (lo..=hi).contains(&c));
        assert_eq!(needs_escape(ch), golden, "U+{c:04X}");
    }
}

#[test]
fn the_six_hostile_names_of_ux57_should_escape_and_decode_back_to_their_bytes() {
    let names: [&[u8]; 6] = [
        "caf\u{e9}".as_bytes(),
        b"a\nb",
        b"a\tb\x1b[2Jc",
        b"bad\xffname",
        b"back\\slash",
        "party \u{1F389}".as_bytes(),
    ];
    for raw in names {
        for tty in [true, false] {
            let esc = escape(raw, tty);
            assert!(!esc.contains('\x1b') && !esc.contains('\n') && !esc.contains('\t'));
            assert_eq!(unescape(&esc), raw.to_vec(), "{esc}");
        }
    }
}

#[test]
fn c1_bidi_zero_width_line_separator_bom_and_tag_characters_should_print_as_u_escapes_in_every_mode(
) {
    let cases: [(&str, &str); 7] = [
        ("a\u{9b}31m", "a\\u{9b}31m"),
        ("a\u{9d}0;t\u{7}", "a\\u{9d}0;t\\x07"),
        ("x\u{202e}gpj", "x\\u{202e}gpj"),
        ("a\u{200b}b", "a\\u{200b}b"),
        ("a\u{2028}b", "a\\u{2028}b"),
        ("a\u{feff}b", "a\\u{feff}b"),
        ("a\u{e0041}b", "a\\u{e0041}b"),
    ];
    for (raw, want) in cases {
        for tty in [true, false] {
            let got = escape(raw.as_bytes(), tty);
            assert_eq!(got, want, "tty={tty}");
            assert!(!got.chars().any(needs_escape) && !got.contains('\x1b'));
            assert_eq!(unescape(&got), raw.as_bytes());
        }
    }
}

#[test]
fn a_zwj_emoji_sequence_should_print_its_joiner_as_an_escape() {
    let family = "\u{1F468}\u{200d}\u{1F469}";
    assert_eq!(
        escape(family.as_bytes(), true),
        "\u{1F468}\\u{200d}\u{1F469}"
    );
}

#[test]
fn one_definition_of_the_escape_function_should_exist_in_the_workspace() {
    let mut defs = Vec::new();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n != "target" && n != "tests") {
                    stack.push(p);
                }
            } else if p.extension().is_some_and(|x| x == "rs") {
                let text = std::fs::read_to_string(&p).unwrap();
                // `fn escape(` exactly: the plan's `grep "fn escape"` also matches `escape_os`, `escape_len`.
                for (i, l) in text.lines().enumerate() {
                    if l.contains("fn escape(") {
                        defs.push(format!("{}:{}", p.display(), i + 1));
                    }
                }
            }
        }
    }
    assert_eq!(defs.len(), 1, "{defs:?}");
    assert!(defs[0].contains("core/src/escape.rs"), "{defs:?}");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn escaping_any_unicode_string_should_hide_every_control_and_decode_back(s in "\\PC*|\\p{Cf}*|.{0,40}") {
        for tty in [true, false] {
            let e = escape(s.as_bytes(), tty);
            prop_assert!(!e.chars().any(needs_escape), "{e:?}");
            prop_assert!(!e.contains('\x1b'));
            if !tty {
                prop_assert!(e.is_ascii());
            }
            prop_assert_eq!(unescape(&e), s.as_bytes().to_vec());
        }
    }

    #[test]
    fn escaping_any_bytes_should_decode_back(b in proptest::collection::vec(any::<u8>(), 0..64)) {
        let e = escape(&b, true);
        prop_assert!(!e.chars().any(needs_escape));
        prop_assert_eq!(unescape(&e), b.clone());
        let _ = OsStr::from_bytes(&b);
    }

    #[test]
    fn a_size_cell_of_any_u64_should_fit_9_characters_and_be_within_5_percent(n in any::<u64>()) {
        let s = fmt_size_cell(n);
        prop_assert!(s.chars().count() <= SIZE_CELL_MAX, "{s}");
        prop_assert!(within_5_percent(&s, n), "{s} vs {n}");
    }

    #[test]
    fn a_size_cell_of_a_log_uniform_u64_should_fit_and_be_close(shift in 0u32..64, low in any::<u64>()) {
        let n = low >> shift;
        let s = fmt_size_cell(n);
        prop_assert!(s.chars().count() <= SIZE_CELL_MAX, "{s}");
        prop_assert!(within_5_percent(&s, n), "{s} vs {n}");
    }
}

fn within_5_percent(cell: &str, bytes: u64) -> bool {
    let (num, unit) = cell.split_once(' ').unwrap();
    let pow = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"]
        .iter()
        .position(|u| *u == unit)
        .unwrap();
    let shown = num.parse::<f64>().unwrap() * (1u128 << (10 * pow)) as f64;
    let truth = bytes as f64;
    if bytes == 0 {
        return shown == 0.0;
    }
    (shown - truth).abs() <= truth * 0.05
}

#[test]
fn size_cells_should_follow_the_table_of_the_story() {
    let t: [(u64, &str); 10] = [
        (0, "0 B"),
        (1, "1 B"),
        (1023, "1.0 KiB"),
        (1024, "1.0 KiB"),
        (1_048_576, "1.0 MiB"),
        (1000 * GIB, "1.0 TiB"),
        (999 * GIB, "999.0 GiB"),
        (GIB * 217 / 10, "21.7 GiB"),
        (u64::MAX, "16.0 EiB"),
        (1000, "1.0 KiB"),
    ];
    for (n, want) in t {
        assert_eq!(fmt_size_cell(n), want, "{n}");
    }
}

#[test]
fn a_value_that_rounds_to_1000_should_step_up_one_unit() {
    // 1023.96 KiB rounds to 1024.0 and 999.96 GiB to 1000.0: both step up.
    assert_eq!(fmt_size_cell(1023 * 1024 + 983), "1.0 MiB");
    assert_eq!(fmt_size_cell(999 * GIB + GIB * 96 / 100), "1.0 TiB");
    assert_eq!(fmt_size_cell(999 * GIB + GIB * 94 / 100), "999.9 GiB");
}

#[test]
fn share_cells_should_follow_the_rounding_rule_for_the_sample_values() {
    let a = share_cell(GIB * 217 / 10, GIB * 336 / 10, 5);
    assert_eq!((a.percent.as_str(), a.filled), ("65%", 3));
    assert_eq!(a.text(), "[###..]  65%");
    let b = share_cell(GIB * 229 / 10, GIB * 2243 / 10, 10);
    assert_eq!((b.percent.as_str(), b.filled), ("10%", 1));
    assert_eq!(b.text(), "[#.........]  10%");
    let c = share_cell(GIB * 6 / 10, GIB * 2243 / 10, 10);
    assert_eq!((c.percent.as_str(), c.filled), ("<1%", 1));
    assert_eq!(c.text(), "[#.........]  <1%");
}

#[test]
fn share_cells_should_round_half_up_and_show_n_a_for_an_empty_parent() {
    // 0.5% is the first percent that does not read "<1%"; 1.5% rounds up to 2%; 2 of 5 cells at exactly 50%.
    assert_eq!(share_cell(5, 1000, 10).percent, "1%");
    assert_eq!(share_cell(499, 100_000, 10).percent, "<1%");
    assert_eq!(share_cell(15, 1000, 10).percent, "2%");
    assert_eq!(share_cell(1, 2, 5).filled, 3);
    assert_eq!(share_cell(1, 4, 10).filled, 3);
    assert_eq!(share_cell(100, 100, 10).text(), "[##########] 100%");
    let none = share_cell(0, 0, 5);
    assert_eq!((none.percent.as_str(), none.filled), ("n/a", 0));
    assert_eq!(none.text(), "[.....]  n/a");
    let zero = share_cell(0, 50, 5);
    assert_eq!((zero.percent.as_str(), zero.filled), ("<1%", 0));
}

proptest! {
    #[test]
    fn a_share_cell_should_stay_inside_its_bar_and_have_a_nonzero_fill_for_nonzero_usage(
        usage in any::<u64>(), parent in any::<u64>(), n in 1usize..20,
    ) {
        let c = share_cell(usage, parent, n);
        prop_assert!(c.filled <= n);
        if parent != 0 && usage > 0 {
            prop_assert!(c.filled >= 1);
        }
        prop_assert_eq!(c.bar().chars().count(), n + 2);
    }
}

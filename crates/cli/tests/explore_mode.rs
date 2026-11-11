#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Story 6.5.4, Task 6.5.4b: the whole truth table of `choose_mode` (UX-T26).

use disk_cleanup_cli::explore::mode::{
    choose_mode, Mode, ModeFlags, MIN_START_COLUMNS, MIN_START_ROWS,
};

const NONE: ModeFlags = ModeFlags {
    text: false,
    json: false,
};

fn auto(stdin: bool, stdout: bool, term: Option<&str>, size: Option<(u16, u16)>) -> Mode {
    choose_mode(stdin, stdout, term, size, NONE, false)
}

#[test]
fn a_full_terminal_should_choose_the_interactive_screen() {
    assert_eq!(
        auto(true, true, Some("xterm-256color"), Some((80, 24))),
        Mode::Interactive
    );
}

#[test]
fn every_missing_terminal_fact_should_choose_the_text_view_with_a_note_that_names_it() {
    let x = Some("xterm");
    let cases = [
        (
            auto(true, false, x, Some((80, 24))),
            "stdout is not a terminal",
        ),
        (
            auto(false, true, x, Some((80, 24))),
            "stdin is not a terminal",
        ),
        (auto(true, true, None, Some((80, 24))), "TERM is not set"),
        (
            auto(true, true, Some(""), Some((80, 24))),
            "TERM is not set",
        ),
        (
            auto(true, true, Some("dumb"), Some((80, 24))),
            "TERM is dumb",
        ),
        (
            auto(true, true, x, None),
            "the terminal size cannot be read",
        ),
        (
            auto(true, true, x, Some((0, 0))),
            "the terminal size cannot be read",
        ),
        (
            auto(true, true, x, Some((80, 0))),
            "the terminal size cannot be read",
        ),
        (auto(true, true, x, Some((1, 1))), "too small"),
        (auto(true, true, x, Some((20, 5))), "too small"),
    ];
    for (mode, why) in cases {
        let Mode::Text { note: Some(note) } = mode else {
            panic!("{why}: not the text view");
        };
        assert!(note.contains(why), "{note}");
        assert!(
            note.starts_with("note: ")
                && note.ends_with("; printing the text view (same as --text)"),
            "{note}"
        );
    }
    assert_eq!(
        auto(true, false, x, Some((80, 24))),
        Mode::Text {
            note: Some(
                "note: stdout is not a terminal; printing the text view (same as --text)"
                    .to_string()
            )
        }
    );
}

#[test]
fn the_smallest_window_that_starts_the_screen_should_be_the_documented_minimum() {
    let size = Some((MIN_START_COLUMNS, MIN_START_ROWS));
    assert_eq!(auto(true, true, Some("xterm"), size), Mode::Interactive);
}

#[test]
fn the_plain_variable_should_choose_the_text_view() {
    let m = choose_mode(true, true, Some("xterm"), Some((80, 24)), NONE, true);
    assert!(matches!(m, Mode::Text { note: Some(n) } if n.contains("DISK_CLEANUP_PLAIN")));
}

#[test]
fn explicit_text_and_json_should_need_no_note_whatever_the_terminal_is() {
    let text = ModeFlags {
        text: true,
        json: false,
    };
    let json = ModeFlags {
        text: false,
        json: true,
    };
    let both = ModeFlags {
        text: true,
        json: true,
    };
    for (stdin, stdout) in [(true, true), (false, false)] {
        let text_mode = choose_mode(stdin, stdout, None, None, text, true);
        assert_eq!(text_mode, Mode::Text { note: None });
        assert_eq!(
            choose_mode(stdin, stdout, None, None, json, true),
            Mode::Json
        );
        assert_eq!(
            choose_mode(stdin, stdout, None, None, both, false),
            Mode::Json
        );
    }
}

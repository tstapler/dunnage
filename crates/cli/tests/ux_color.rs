#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.2.3: colour and terminal rules (UX-30 to UX-32, UX-36).

mod support;

use std::collections::HashMap;

use disk_cleanup_cli::render::render_plan;
use disk_cleanup_cli::render_json::{plan_doc, to_text};
use disk_cleanup_cli::terminal::{color_on, ColorChoice, Terminal};
use disk_cleanup_cli::view::PlanView;
use support::*;

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect();
    move |k| m.get(k).cloned()
}

#[test]
fn output_should_contain_no_0x1b_under_no_color_color_never_term_dumb_pipe_and_color_only_on_tty_or_always_or_clicolor_force(
) {
    use ColorChoice::{Always, Auto, Never};
    type Case<'a> = (ColorChoice, bool, Vec<(&'a str, &'a str)>, bool);
    let rows: Vec<Case> = vec![
        (Auto, true, vec![], true),
        (Auto, false, vec![], false),
        (Auto, true, vec![("NO_COLOR", "1")], false),
        (Auto, true, vec![("NO_COLOR", "")], true),
        (Never, true, vec![("CLICOLOR_FORCE", "1")], false),
        (Always, false, vec![("NO_COLOR", "1")], true),
        (Auto, false, vec![("CLICOLOR_FORCE", "1")], true),
        (Auto, false, vec![("CLICOLOR_FORCE", "0")], false),
        (Auto, true, vec![("CLICOLOR_FORCE", "0")], true),
        (Auto, true, vec![("TERM", "dumb")], false),
        (
            Auto,
            false,
            vec![("TERM", "dumb"), ("CLICOLOR_FORCE", "1")],
            true,
        ),
        (
            Auto,
            true,
            vec![("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")],
            false,
        ),
    ];
    let fx = Fixture::s1();
    let input = fx.input();
    let view = PlanView::build(&input);
    for (choice, tty, env, want) in rows {
        let on = color_on(choice, tty, &env_of(&env));
        assert_eq!(on, want, "{choice:?} tty={tty} {env:?}");
        let term = Terminal {
            tty,
            width: 80,
            color: on,
        };
        let out = render_plan(&input, &view, &term);
        assert_eq!(out.contains('\x1b'), want, "{choice:?} tty={tty} {env:?}");
    }
    let json = to_text(&plan_doc(&input, &view, 0));
    assert!(!json.contains('\x1b'));
}

#[test]
fn every_status_should_be_a_word_when_ansi_stripped_and_screenshot_greyscale_loses_nothing() {
    let fx = Fixture::s1();
    let input = fx.input();
    let view = PlanView::build(&input);
    let plain = render_plan(&input, &view, &pipe(80));
    let coloured = render_plan(
        &input,
        &view,
        &Terminal {
            tty: true,
            width: 80,
            color: true,
        },
    );
    let mut stripped = String::new();
    let mut chars = coloured.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n == 'm' {
                    break;
                }
            }
        } else {
            stripped.push(c);
        }
    }
    assert_eq!(
        stripped, plain,
        "colour only tints words; the text is unchanged"
    );
    for l in plain
        .lines()
        .skip_while(|l| !l.starts_with("STATUS"))
        .skip(1)
    {
        if l.starts_with(' ') || l.starts_with("DRY") {
            continue;
        }
        let w = l.split_whitespace().next().unwrap();
        assert!(["FREE", "RISK", "SKIP", "ERR", "FREED"].contains(&w), "{l}");
    }
}

#[test]
fn non_tty_output_should_have_one_record_per_line_no_cr_no_box_drawing_no_non_ascii_no_progress_on_stdout(
) {
    let fx = Fixture::s1();
    let input = fx.input();
    let out = render_plan(&input, &PlanView::build(&input), &pipe(80));
    assert!(out.is_ascii());
    assert!(!out.contains('\r') && !out.contains('\x1b') && !out.contains('\x08'));
    assert!(out.bytes().all(|b| b == b'\n' || (0x20..0x7f).contains(&b)));
    assert!(!out.contains("Scanning") && !out.contains("..."));
}

#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.2.3, Task 1.2.3c: the width source and the layout by width (UX-45, T-441).

mod support;

use std::collections::HashMap;

use disk_cleanup_cli::render::render_plan;
use disk_cleanup_cli::terminal::{ColorChoice, Terminal};
use disk_cleanup_cli::view::PlanView;
use support::*;

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect();
    move |k| m.get(k).cloned()
}

fn render_at(width: usize) -> String {
    let fx = Fixture::s1();
    let input = fx.input();
    render_plan(&input, &PlanView::build(&input), &pipe(width))
}

fn table_header(out: &str) -> String {
    out.lines()
        .find(|l| l.starts_with("STATUS"))
        .unwrap()
        .to_string()
}

#[test]
fn width_should_come_from_the_ioctl_on_a_pty_columns_in_a_pipe_and_80_otherwise() {
    let t = |tty, ioctl, env: &[(&str, &str)]| {
        Terminal::resolve(tty, ioctl, ColorChoice::Auto, &env_of(env)).width
    };
    assert_eq!(t(true, Some(132), &[("COLUMNS", "50")]), 132);
    assert_eq!(t(false, Some(132), &[("COLUMNS", "59")]), 59);
    assert_eq!(
        t(false, None, &[("COLUMNS", "10")]),
        80,
        "COLUMNS=10 is ignored"
    );
    assert_eq!(t(false, None, &[("COLUMNS", "abc")]), 80);
    assert_eq!(t(false, None, &[]), 80);
    assert_eq!(t(true, None, &[("COLUMNS", "100")]), 100);
    assert_eq!(t(true, Some(5), &[]), 20, "never narrower than 20");
}

#[test]
fn layout_should_follow_the_width_source_and_never_truncate_anything_at_120_90_89_80_59_39() {
    for w in [120, 90, 89, 80, 59, 39] {
        let out = render_at(w);
        let head = table_header(&out);
        assert_eq!(head.contains("TIER"), w >= 90, "TIER at {w}: {head}");
        assert_eq!(head.contains("AGE"), w >= 60, "AGE at {w}: {head}");
        assert!(!out.contains('\r') && out.is_ascii());
        for l in out
            .lines()
            .filter(|l| !l.starts_with("  disk-cleanup") && !l.starts_with("  sudo"))
        {
            assert!(l.chars().count() <= w, "{w}: {l}");
        }
        // nothing truncated: the long Steam path survives wrapping
        let flat: String = out
            .lines()
            .skip_while(|l| !l.starts_with("STATUS"))
            .filter(|l| l.starts_with("  ") || l.contains("Steam"))
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("");
        assert!(
            flat.replace(' ', "").contains("compatdata/440"),
            "{w}\n{out}"
        );
    }
    let narrow = render_at(39);
    assert_eq!(table_header(&narrow), "STATUS  SIZE");
    let rows: Vec<&str> = narrow
        .lines()
        .skip_while(|l| !l.starts_with("STATUS"))
        .collect();
    assert_eq!(rows[1], "  KIND");
    assert!(
        rows[2].starts_with("RISK ") && rows[2].ends_with("GiB"),
        "{}",
        rows[2]
    );
    assert_eq!(rows[3], "  build-dirs");
}

#[test]
fn blank_cells_should_read_n_a_never_a_dash() {
    let out = render_at(80);
    assert!(out.contains("n/a"));
    for l in out
        .lines()
        .filter(|l| l.starts_with("FREE") || l.starts_with("RISK"))
    {
        assert!(!l.split_whitespace().any(|w| w == "-"), "{l}");
    }
}

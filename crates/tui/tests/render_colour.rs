#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Colour is decoration (Task 6.5.3c; UX-T25) and hostile or wide names never reach the terminal raw
//! (UX-T30): byte-level checks over `CrosstermBackend<Vec<u8>>` and cell-level checks over `TestBackend`.

mod common;

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

use common::*;
use disk_cleanup_core::escape::needs_escape;
use disk_cleanup_core::explore::layout::cells;
use disk_cleanup_core::explore::msg::{NodeEvent, ScanMode, NO_PARENT};
use disk_cleanup_core::explore::rows::Badge;
use disk_cleanup_core::explore::tree::ExploreTree;
use disk_cleanup_core::snapshot_types::FsSnapshot;
use disk_cleanup_tui::terminal::fixed_terminal;
use disk_cleanup_tui::theme::{color_on, ColorMode};
use disk_cleanup_tui::view::render;
use ratatui::backend::CrosstermBackend;

/// A writer the test can read back after the backend took ownership of a clone.
#[derive(Clone, Default)]
struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |k| {
        pairs
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| (*v).to_string())
    }
}

#[test]
fn colour_follows_the_cleaners_precedence_rules() {
    use ColorMode::{Always, Auto, Never};
    let none: &[(&str, &str)] = &[];
    // --color beats everything
    assert!(color_on(Always, false, &env(&[("NO_COLOR", "1")])));
    assert!(!color_on(Never, true, &env(&[("CLICOLOR_FORCE", "1")])));
    // a non-empty NO_COLOR beats CLICOLOR_FORCE, TERM and the TTY
    assert!(!color_on(
        Auto,
        true,
        &env(&[("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")])
    ));
    assert!(
        color_on(Auto, true, &env(&[("NO_COLOR", "")])),
        "an empty NO_COLOR is unset"
    );
    // CLICOLOR_FORCE beats TERM=dumb and a pipe; 0 does not force
    assert!(color_on(
        Auto,
        false,
        &env(&[("CLICOLOR_FORCE", "1"), ("TERM", "dumb")])
    ));
    assert!(!color_on(Auto, false, &env(&[("CLICOLOR_FORCE", "0")])));
    // TERM=dumb beats the TTY
    assert!(!color_on(Auto, true, &env(&[("TERM", "dumb")])));
    // otherwise the TTY decides
    assert!(color_on(Auto, true, &env(none)));
    assert!(!color_on(Auto, false, &env(none)));
}

/// The colour parameters of every SGR sequence in `bytes`. Resets (39, 49, 59, 0) are not colours:
/// ratatui writes `ESC[39m ESC[49m ESC[59m ESC[0m` after every frame.
fn colour_params(bytes: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'm' {
                let params = String::from_utf8_lossy(&bytes[i + 2..j]).into_owned();
                let mut it = params.split(';').peekable();
                while let Some(p) = it.next() {
                    let n: u32 = p.parse().unwrap_or(0);
                    let colour = matches!(n, 30..=37 | 40..=47 | 90..=97 | 100..=107);
                    if colour || n == 38 || n == 48 || n == 58 {
                        found.push(params.clone());
                    }
                    if n == 38 || n == 48 || n == 58 {
                        break;
                    }
                    let _ = it.peek();
                }
            }
            i = j;
        }
        i += 1;
    }
    found
}

fn bytes_of(f: &Fixture, w: u16, h: u16) -> Vec<u8> {
    let sink = Shared::default();
    let mut term = fixed_terminal(CrosstermBackend::new(sink.clone()), w, h).unwrap();
    term.draw(|fr| render(fr, &f.screen())).unwrap();
    let bytes = sink.0.lock().unwrap().clone();
    bytes
}

fn states() -> Vec<Fixture> {
    let mut v = Vec::new();
    for finished in [false, true] {
        let mut f = Fixture::new(sample_tree(finished)).with_badge(|t, id| {
            if t.name(id) == "node_modules" {
                Badge::Candidate
            } else {
                Badge::None
            }
        });
        f.mark("/home/u/code/app/node_modules", GIB);
        v.push(f);
    }
    // a CRIT headroom line
    let mut crit = Fixture::new(sample_tree(true));
    let mut snap: FsSnapshot = btrfs_snapshot();
    snap.btrfs.as_mut().unwrap().unallocated = 100 * MIB;
    crit.snapshot = Ok(snap);
    v.push(crit);
    v
}

#[test]
fn with_colour_off_no_byte_carries_a_colour_sgr_but_reverse_and_bold_remain() {
    for f in states() {
        for (w, h) in [(120u16, 24u16), (80, 24), (60, 14)] {
            let bytes = bytes_of(&f, w, h);
            assert!(!bytes.is_empty());
            assert_eq!(colour_params(&bytes), Vec::<String>::new(), "{w}x{h}");
            let text = String::from_utf8_lossy(&bytes);
            assert!(text.contains("\x1b[7m"), "reverse video for the cursor row");
            assert!(text.contains("\x1b[1m"), "bold for the verdict word");
        }
        // and no cell carries a colour either
        let term = draw(&f.screen(), 100, 24);
        for c in &term.backend().buffer().content {
            assert_eq!(c.fg, ratatui::style::Color::Reset);
            assert_eq!(c.bg, ratatui::style::Color::Reset);
        }
    }
}

#[test]
fn with_colour_on_only_the_verdict_word_and_done_rows_are_tinted_and_the_words_do_not_change() {
    let mut on = Fixture::new(sample_tree(true));
    on.color = true;
    let off = Fixture::new(sample_tree(true));
    assert_eq!(
        draw_text(&on.screen(), 100, 24),
        draw_text(&off.screen(), 100, 24),
        "text is identical"
    );
    let term = draw(&on.screen(), 100, 24);
    let buf = term.backend().buffer();
    let coloured: Vec<(u16, u16)> = (0..24u16)
        .flat_map(|y| (0..100u16).map(move |x| (x, y)))
        .filter(|&(x, y)| buf[(x, y)].fg != ratatui::style::Color::Reset)
        .collect();
    assert!(!coloured.is_empty());
    let rows: std::collections::BTreeSet<u16> = coloured.iter().map(|c| c.1).collect();
    // the headroom line (row 1) and the done directory rows (5..), never the title, header, cursor row or footer
    assert!(rows.contains(&1));
    assert!(
        rows.iter().any(|r| (5..12).contains(r)),
        "done rows are tinted: {rows:?}"
    );
    assert!(!rows.contains(&0) && !rows.contains(&3) && !rows.contains(&4) && !rows.contains(&22));
    let verdict_cells: Vec<_> = coloured.iter().filter(|c| c.1 == 1).collect();
    assert_eq!(
        verdict_cells.len(),
        "WARN".len(),
        "only the verdict word on the headroom line"
    );
    let bytes = bytes_of(&on, 100, 24);
    assert!(!colour_params(&bytes).is_empty());
}

fn hostile_names() -> Vec<OsString> {
    let b = |v: &[u8]| OsString::from_vec(v.to_vec());
    vec![
        b(b"plain"),
        b(b"a\nb"),
        b(b"a\tb\x1b[2Jc"),
        b(b"$(touch INJECTED)"),
        b(b"a'; touch INJECTED; '"),
        b(b"bad\xffbyte"),
        b("a\u{9b}31m".as_bytes()),
        b("a\u{9d}0;t\x07".as_bytes()),
        b("x\u{202e}gpj".as_bytes()),
        b("\u{200b}".as_bytes()),
        b("\u{4e2d}\u{6587}\u{5b57}\u{7b26}".as_bytes()),
        b("\u{1f4a5}\u{1f525}boom".as_bytes()),
        b(b"-rf"),
    ]
}

fn hostile_tree() -> ExploreTree {
    let names = hostile_names();
    let mut ev = vec![dir_ev(
        0,
        NO_PARENT,
        "/r",
        stats(0, names.len() as u64, 4096, 0),
        names.len() as u32,
        0,
    )];
    for (i, n) in names.iter().enumerate() {
        ev.push(dir_named(
            1 + i as u64,
            0,
            n.clone(),
            stats(1, 0, 4096 * (100 - i as u64), 10),
        ));
    }
    let mut t = ExploreTree::new();
    t.apply_batch(0, ScanMode::Fresh, ev).unwrap();
    t.finish(false);
    t
}

#[test]
fn hostile_and_wide_names_never_reach_a_cell_raw_and_every_row_fits_the_width() {
    for (w, h) in [(120u16, 30u16), (100, 24), (80, 24), (60, 20)] {
        let mut f = Fixture::new(hostile_tree());
        f.state.input.show_info = false;
        // visit every row with the cursor so every name is drawn under it too
        for step in 0..hostile_names().len() {
            f.state.view.move_to(step, 50_000);
            let term = draw(&f.screen(), w, h);
            for c in &term.backend().buffer().content {
                for ch in c.symbol().chars() {
                    assert!(
                        !needs_escape(ch),
                        "{w}x{h}: raw {ch:?} in cell {:?}",
                        c.symbol()
                    );
                }
            }
            let text = buffer_text(term.backend().buffer());
            assert_fits(&text, usize::from(w));
            if w >= 100 {
                assert!(
                    text.contains("a\\x1b[2Jc") || text.contains("a\\tb\\x1b[2Jc"),
                    "{text}"
                );
            }
        }
    }
    let f = Fixture::new(hostile_tree());
    let text = draw_text(&f.screen(), 200, 30);
    for want in [
        "a\\nb/",
        "a\\u{9b}31m/",
        "a\\u{9d}0;t\\x07/",
        "x\\u{202e}gpj/",
        "\\u{200b}/",
        "bad\\xffbyte/",
        "\u{4e2d}\u{6587}\u{5b57}\u{7b26}/",
        "\u{1f4a5}\u{1f525}boom/",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
}

#[test]
fn the_footer_cuts_a_long_path_from_the_left_and_the_info_pane_wraps_it_in_full() {
    let long = "d".repeat(70);
    let ev = vec![
        dir_ev(0, NO_PARENT, "/home/u/code", stats(0, 1, 4096, 0), 1, 0),
        dir_ev(1, 0, &long, stats(1, 0, 4096, 10), 0, 0),
    ];
    let mut t = ExploreTree::new();
    t.apply_batch(0, ScanMode::Fresh, ev).unwrap();
    t.finish(false);
    let mut f = Fixture::new(t);
    f.state.view.move_to(1, 1);
    let text = draw_text(&f.screen(), 60, 30);
    let footer = text.lines().rev().nth(2).unwrap();
    assert!(footer.starts_with("Path: ..."), "{footer:?}");
    assert!(footer.ends_with(&long[..20]), "{footer:?}");
    assert!(cells(footer) <= 60);
    f.state.input.show_info = true;
    let text = draw_text(&f.screen(), 60, 30);
    let flat: String = text
        .lines()
        .skip_while(|l| !l.starts_with("-- Info"))
        .take(4)
        .collect::<Vec<_>>()
        .join("");
    assert!(
        flat.replace(['-', ' '], "").contains(&long),
        "the whole path is in the pane: {text}"
    );
}

#[test]
fn the_boundary_message_command_delivers_each_hostile_name_as_one_argument() {
    use disk_cleanup_core::shell_quote::sandbox::run_printed_line;
    let mut delivered = 0;
    for tty in [true, false] {
        for (i, name) in hostile_names().into_iter().enumerate() {
            let ev = vec![
                dir_ev(0, NO_PARENT, "/r", stats(0, 1, 4096, 0), 1, 0),
                NodeEvent::Boundary {
                    id: 1,
                    parent: 0,
                    name: name.clone(),
                    dev: 9,
                },
            ];
            let mut t = ExploreTree::new();
            t.apply_batch(0, ScanMode::Fresh, ev).unwrap();
            let mut f = Fixture::new(t);
            f.state.view.move_to(1, 1);
            let mut s = f.screen();
            s.tty = tty;
            let text = draw_text(&s, 400, 24);
            let Some(cmd) = text
                .lines()
                .find_map(|l| l.split("From the shell: ").nth(1))
            else {
                panic!("no command printed for {name:?}: {text}");
            };
            for shell in ["bash", "zsh"] {
                let dir = std::env::temp_dir().join(format!(
                    "tui-boundary-{}-{shell}-{tty}-{i}",
                    std::process::id()
                ));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                let run = run_printed_line(&dir, shell, cmd.trim()).unwrap().unwrap();
                let want = {
                    let mut p = b"/r/".to_vec();
                    p.extend_from_slice(std::os::unix::ffi::OsStrExt::as_bytes(name.as_os_str()));
                    p
                };
                assert_eq!(
                    run.argv_of("disk-cleanup").map(<[Vec<u8>]>::to_vec),
                    Some(vec![b"explore".to_vec(), want]),
                    "{shell} tty={tty}: {cmd:?}"
                );
                assert!(run.injected.is_empty() && !dir.join("INJECTED").exists());
                let _ = std::fs::remove_dir_all(&dir);
                delivered += 1;
            }
        }
    }
    assert_eq!(delivered, 2 * 2 * hostile_names().len());
}

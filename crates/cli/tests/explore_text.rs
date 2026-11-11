#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Story 6.5.5: the `--text` view over a scripted scan.

mod explore_fixture;

use std::sync::Arc;
use std::time::Duration;

use disk_cleanup_cli::explore::text::{run_text, INTERRUPTED};
use disk_cleanup_core::explore::testing::FakeTermSignals;
use disk_cleanup_core::shell_quote::sandbox::{find_on_path, run_printed_line};
use explore_fixture::{env, golden};

fn text(width: usize, truncated: bool, top: usize) -> (i32, String, String) {
    let mut e = env(width, truncated);
    e.top = top;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_text(&e, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn the_text_view_at_80_columns_should_match_the_golden_wireframe_shape() {
    let (code, out, err) = text(80, false, 7);
    assert_eq!(code, 0);
    assert_eq!(err, "");
    golden("explore_text_80.txt", &out);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "EXPLORE (text) - read-only. Nothing was deleted.");
    assert_eq!(
        *lines.last().unwrap(),
        "EXPLORE CLOSED - nothing was deleted."
    );
    assert!(
        out.contains("USAGE   APPARENT  META(est)    ITEMS  SHARE  NAME"),
        "{out}"
    );
    for word in [
        "[build-dirs inside]",
        "[unreadable: EACCES, at least]",
        "[other filesystem: not scanned]",
    ] {
        assert!(out.contains(word), "{word} missing:\n{out}");
    }
}

#[test]
fn the_output_should_be_ascii_with_no_escape_byte_or_carriage_return() {
    let (_, out, _) = text(80, false, 0);
    assert!(out.is_ascii(), "{out}");
    assert!(!out.bytes().any(|b| b == 0x1b || b == b'\r'));
    assert!(out.contains("caf\\u{e9}/"), "{out}");
    assert!(out.contains("bad\\xffname/"), "{out}");
}

#[test]
fn no_line_should_pass_the_width_except_a_next_line_and_nothing_should_be_cut() {
    let (_, wide, _) = text(200, false, 0);
    let names: Vec<&str> = [
        "old-app/",
        "app/",
        "root-owned/",
        "caf\\u{e9}/",
        "bad\\xffname/",
        "mnt/",
    ]
    .to_vec();
    for width in 20..=200 {
        let (_, out, _) = text(width, false, 0);
        for l in out.lines().filter(|l| !l.starts_with("Next:")) {
            assert!(l.chars().count() <= width, "width {width}: {l:?}");
        }
        // Nothing is truncated: every name survives wrapping, whole, once the line breaks are removed.
        let joined: String = out.lines().map(str::trim).collect::<Vec<_>>().join("");
        let squashed: String = joined.split_whitespace().collect();
        for n in &names {
            assert!(wide.contains(n));
            assert!(squashed.contains(n), "width {width}: {n} cut");
        }
    }
}

#[test]
fn bars_should_appear_only_with_the_flag() {
    let (_, plain, _) = text(120, false, 0);
    assert!(!plain.contains("[#"));
    let mut e = env(120, false);
    e.bars = true;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    run_text(&e, &mut out, &mut err);
    assert!(String::from_utf8(out).unwrap().contains("[#"));
}

#[test]
fn depth_two_should_print_the_path_relative_to_the_root() {
    let mut e = env(120, false);
    e.depth = 2;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    run_text(&e, &mut out, &mut err);
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("app/node_modules/"), "{out}");
}

#[test]
fn a_partial_scan_should_be_marked_and_still_exit_zero() {
    let mut e = env(80, true);
    e.max_seconds = Some(1);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_text(&e, &mut out, &mut err);
    let out = String::from_utf8(out).unwrap();
    assert_eq!(code, 0);
    assert!(out.contains("PARTIAL SCAN"), "{out}");
}

#[test]
fn a_signal_during_the_scan_should_print_the_interrupted_line_and_exit_130() {
    let mut e = env(80, false);
    e.signals = Arc::new(FakeTermSignals::at_tick(1, 2));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_text(&e, &mut out, &mut err);
    assert_eq!(code, 130);
    assert!(out.is_empty());
    assert_eq!(String::from_utf8(err).unwrap().trim(), INTERRUPTED);
}

#[test]
fn progress_should_go_to_the_progress_sink_not_stdout() {
    use std::sync::Mutex;
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let s2 = Arc::clone(&seen);
    let mut e = env(80, false);
    e.progress = Some(Arc::new(move |l: &str| {
        s2.lock().unwrap().push(l.to_string());
    }));
    e.progress_every = Duration::from_millis(0);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    run_text(&e, &mut out, &mut err);
    assert!(!String::from_utf8(out).unwrap().contains("scanning:"));
}

#[test]
fn the_next_lines_should_pass_sh_n_carry_no_annotation_and_deliver_names_as_one_argument() {
    let (_, out, _) = text(80, false, 0);
    let nexts: Vec<&str> = out.lines().filter(|l| l.starts_with("Next: ")).collect();
    assert!(
        nexts.contains(&"Next: disk-cleanup --kinds build-dirs"),
        "{out}"
    );
    assert!(
        nexts
            .iter()
            .any(|l| l.starts_with("Next: disk-cleanup explore ")),
        "{out}"
    );
    for l in &nexts {
        assert!(!l.contains("(look inside"));
        assert!(!l.contains("--root"));
    }
    // The explore line for each hostile directory name, run in real shells.
    for (name, bytes) in [
        ("caf\u{e9}", "caf\u{e9}".as_bytes().to_vec()),
        ("bad", b"bad\xffname".to_vec()),
    ] {
        let q = disk_cleanup_core::shell_quote::shell_quote(
            std::os::unix::ffi::OsStrExt::from_bytes(&[b"/h/code/".as_slice(), &bytes].concat()),
            true,
        );
        assert!(q.is_ascii(), "{name}");
        let line = format!("disk-cleanup explore {q}");
        for shell in ["bash", "zsh"] {
            if find_on_path(shell).is_none() {
                continue;
            }
            let dir = tempfile::tempdir().unwrap();
            let run = run_printed_line(dir.path(), shell, &line).unwrap().unwrap();
            let argv = run.argv_of("disk-cleanup").unwrap();
            assert_eq!(argv.len(), 2);
            assert_eq!(argv[1], [b"/h/code/".as_slice(), &bytes].concat());
            assert!(run.injected.is_empty());
        }
    }
}

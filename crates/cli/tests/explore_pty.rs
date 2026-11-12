#![cfg(feature = "explore")]
#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.5.6, Task 6.5.6a (Gate F, UX-T1, UX-T23, UX-T34, UX-T35): the real `disk-cleanup explore` binary on a
//! pty. After every way the session can end the transcript shows the alternate screen left and the cursor
//! shown, `stty -a` equals the state saved before, and nothing but the closing text stands outside the
//! alternate screen; a name holding `ESC[2J` never reaches the transcript raw; and a script that presses every
//! key except the confirmed `d` path leaves the tree byte-identical and writes no audit line.
//! Everything lives in a tempdir fixture HOME.

mod common;
mod explore_pty_rig;

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use common::{bin_path, tree_manifest, Fixture};
use explore_pty_rig::{count, find, rfind, Pty, Spawn, ENTER_ALT, LEAVE_ALT};

const CTRL_C: &[u8] = &[0x03];
const CTRL_Z: &[u8] = &[0x1a];
const DELETE_KEY: &[u8] = b"\x1b[3~";
const ESC: &[u8] = b"\x1b";
const ENTER: &[u8] = b"\r";
/// The title once the scan ended; the diff painter may split "DONE" on the wire, so the marker stops short.
const SCAN_DONE: &str = "SCAN D";

/// `<fixture>/home/code/{app/{node_modules,src},lib}` and a directory whose name carries `ESC[2J`.
fn tree(f: &Fixture) -> PathBuf {
    let code = f.home().join("code");
    for (dir, files) in [
        ("app/node_modules/pkg", 3),
        ("app/src", 3),
        ("lib", 4),
        ("notes", 2),
    ] {
        fs::create_dir_all(code.join(dir)).unwrap();
        for i in 0..files {
            fs::write(
                code.join(dir).join(format!("f{i}")),
                vec![b'x'; 3000 * (i + 1)],
            )
            .unwrap();
        }
    }
    fs::write(code.join("app/package.json"), "{}").unwrap();
    fs::canonicalize(code).unwrap()
}

fn evil_name_dir(root: &std::path::Path) {
    let evil = root.join("evil\u{1b}[2Jname");
    fs::create_dir_all(&evil).unwrap();
    fs::write(evil.join("f"), vec![b'y'; 5000]).unwrap();
}

fn spawn(f: &Fixture, root: &std::path::Path) -> Pty {
    Spawn::new(f, &[root]).run(&bin_path())
}

/// Waits for the finished scan, which is also when the first frame and the keys are live.
fn up(p: &mut Pty) {
    p.wait_for(SCAN_DONE);
    std::thread::sleep(Duration::from_millis(200));
    p.pump();
}

/// The shared tail of every journey: the shell reached its last `stty`, the terminal is restored and the text
/// outside the alternate screen is only the reserve-file note, the closing line and the exit marker.
fn assert_clean_end(p: &mut Pty) {
    p.finish();
    p.assert_restored();
    assert_only_closing_text(p);
}

fn assert_only_closing_text(p: &Pty) {
    let outside = p.outside_alt();
    for line in outside.lines() {
        let line = line.trim();
        let allowed = line.is_empty()
            || line.starts_with("EXIT=")
            || line.starts_with("note: created 2 reserve files")
            || line.starts_with("panicked at ")
            || line.starts_with("EXPLORE CLOSED")
            || line.starts_with("Closed while scanning")
            || line.starts_with("Interrupted")
            || line.starts_with("thread '")
            || line.contains("forced panic in mark_check")
            || line.starts_with("note: run with `RUST_BACKTRACE");
        assert!(
            allowed,
            "unexpected text outside the alternate screen: {line:?}\nfull: {outside:?}"
        );
    }
    assert!(
        !String::from_utf8_lossy(&p.seen).contains("PROMPT> ") || outside.contains("EXIT="),
        "the session never ended"
    );
}

#[test]
fn q_after_the_scan_should_restore_print_the_closing_line_and_exit_0() {
    let f = Fixture::new();
    let root = tree(&f);
    let mut p = spawn(&f, &root);
    up(&mut p);
    p.send(b"q");
    assert_eq!(p.exit_code(), 0);
    assert_clean_end(&mut p);
    let outside = p.outside_alt();
    assert!(
        outside.contains("EXPLORE CLOSED - nothing was deleted."),
        "{outside:?}"
    );
    let leave = rfind(&p.seen, LEAVE_ALT).unwrap();
    let closing = find(&p.seen, b"EXPLORE CLOSED").unwrap();
    assert!(closing > leave, "the closing line follows the restore");
}

#[test]
fn the_ctrl_c_key_should_restore_and_exit_130() {
    let f = Fixture::new();
    let root = tree(&f);
    let mut p = spawn(&f, &root);
    up(&mut p);
    p.send(CTRL_C);
    assert_eq!(p.exit_code(), 130);
    assert_clean_end(&mut p);
}

fn external_signal(sig: &str, want: i32) {
    let f = Fixture::new();
    let root = tree(&f);
    let mut p = spawn(&f, &root);
    up(&mut p);
    p.kill(sig);
    assert_eq!(p.exit_code(), want, "{sig}");
    assert_clean_end(&mut p);
}

#[test]
fn sigterm_should_restore_and_exit_143() {
    external_signal("TERM", 143);
}

#[test]
fn sighup_should_restore_and_exit_143() {
    external_signal("HUP", 143);
}

#[test]
fn an_external_sigint_should_restore_and_exit_130() {
    external_signal("INT", 130);
}

#[test]
fn ctrl_z_then_fg_should_restore_stop_reenter_and_redraw() {
    let f = Fixture::new();
    let root = tree(&f);
    let mut p = Spawn::new(&f, &[&root]).job_control().run(&bin_path());
    up(&mut p);
    let before = p.seen.len();
    p.send(CTRL_Z);
    p.wait_bytes(LEAVE_ALT, before);
    p.wait_for_after("Stopped", before);
    let stopped_at = p.seen.len();
    std::thread::sleep(Duration::from_millis(300));
    p.send(b"fg\n");
    p.wait_bytes(ENTER_ALT, stopped_at);
    p.wait_for_after(SCAN_DONE, stopped_at);
    p.send(b"q");
    p.wait_for_after("EXPLORE CLOSED", stopped_at);
    let closed_at = find(&p.seen, b"EXPLORE CLOSED").unwrap();
    p.after_job(closed_at);
    assert_eq!(p.exit_code(), 0);
    p.finish();
    p.assert_restored();
    assert_eq!(count(&p.seen, ENTER_ALT), 2, "left once and entered again");
}

#[test]
fn a_resize_should_redraw_the_frame_and_a_tiny_window_should_say_it_is_too_small() {
    let f = Fixture::new();
    let root = tree(&f);
    let mut p = spawn(&f, &root);
    up(&mut p);
    let from = p.seen.len();
    p.session.get_process_mut().set_window_size(50, 10).unwrap();
    p.wait_for_after("Too small: need 60x12, have 50x10.", from);
    let from = p.seen.len();
    p.session
        .get_process_mut()
        .set_window_size(120, 40)
        .unwrap();
    p.wait_for_after(SCAN_DONE, from);
    p.send(b"q");
    assert_eq!(p.exit_code(), 0);
    assert_clean_end(&mut p);
}

#[test]
fn a_forced_panic_on_the_ui_thread_should_restore_first_and_then_print_the_message() {
    let f = Fixture::new();
    let root = tree(&f);
    let mut p = Spawn::new(&f, &[&root])
        .env("DC_EXPLORE_PANIC_AT", "mark_check")
        .run(&bin_path());
    up(&mut p);
    p.send(b" ");
    assert_eq!(p.exit_code(), 101, "{}", p.tail());
    assert_clean_end(&mut p);
    let leave = rfind(&p.seen, LEAVE_ALT).unwrap();
    let msg = find(&p.seen, b"forced panic in mark_check").expect("the panic message is printed");
    assert!(msg > leave, "the message follows the restore");
}

#[test]
fn a_name_holding_esc_2j_should_never_reach_the_transcript_raw() {
    let f = Fixture::new();
    let root = tree(&f);
    evil_name_dir(&root);
    let mut p = spawn(&f, &root);
    up(&mut p);
    p.keys(&[b"j", b"j", b"j", b"j", b"j"]);
    p.send(b"q");
    assert_eq!(p.exit_code(), 0);
    assert_clean_end(&mut p);
    let seen = &p.seen;
    assert!(
        find(seen, b"evil\x1b").is_none(),
        "the raw name reached the terminal: {:?}",
        String::from_utf8_lossy(seen)
    );
    assert!(
        String::from_utf8_lossy(seen).contains("evil"),
        "the name is shown, escaped"
    );
}

/// Presses `D`, Delete, `y`, Enter, `x`, `p`, Space and `d` on every row; with the delete wired (`gate-d`) it
/// also takes `d` with marks and no matching plan, a wrong count and Esc at the prompt.
#[test]
fn the_no_delete_script_should_leave_the_tree_byte_identical_and_write_no_audit_line() {
    let f = Fixture::new();
    let root = tree(&f);
    evil_name_dir(&root);
    let before = tree_manifest(&f.home());
    let mut p = spawn(&f, &root);
    up(&mut p);
    for _row in 0..7 {
        p.keys(&[b"D", DELETE_KEY, b"y", b"x", b"p", ESC, b" ", b"d", ESC]);
        p.keys(&[b"j"]);
    }
    p.keys(&[b"D", DELETE_KEY, b"y", ENTER, b"x", ESC]);
    p.send(CTRL_C);
    assert_eq!(p.exit_code(), 130, "{}", p.tail());
    assert_clean_end(&mut p);
    assert_eq!(tree_manifest(&f.home()), before, "the tree changed");
    let audit = f.xdg("state").join("disk-cleanup/audit.jsonl");
    assert!(!audit.exists(), "an audit line was written");
    let text = String::from_utf8_lossy(&p.seen).into_owned();
    assert!(
        !text.contains("deleted 1") && !text.contains("DELETING"),
        "{text:?}"
    );
}

/// `d` with a mark and no matching plan only plans; `d` again opens the prompt; a wrong count and Esc delete
/// nothing (UX-T34, UX-T35). Needs a build that registers `explorer-selection`.
#[cfg(feature = "gate-d")]
#[test]
fn d_then_a_wrong_count_then_esc_should_delete_nothing_and_write_no_audit_line() {
    let f = Fixture::new();
    let root = tree(&f);
    let before = tree_manifest(&f.home());
    let mut p = spawn(&f, &root);
    up(&mut p);
    p.keys(&[b"j", b" ", b"d"]);
    p.wait_for("Review the plan, then press d again.");
    let from = p.seen.len();
    p.keys(&[b"d"]);
    p.wait_for_after("Type 1 and press Enter to delete", from);
    p.keys(&[b"9", b"9", ENTER]);
    p.wait_for_after("That is not 1. Nothing was deleted.", from);
    p.keys(&[ESC]);
    p.send(CTRL_C);
    assert_eq!(p.exit_code(), 130, "{}", p.tail());
    assert_clean_end(&mut p);
    assert_eq!(tree_manifest(&f.home()), before, "the tree changed");
    assert!(!f.xdg("state").join("disk-cleanup/audit.jsonl").exists());
}

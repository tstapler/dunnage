#![cfg(feature = "explore")]
#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.5.6, Task 6.5.6c (Gate F, UX-T22): `explore` on a full disk. `scripts/rehearse-full-disk.sh
//! --cache-too` mounts two full 24 MiB tmpfs filesystems inside `unshare -rm` for `XDG_STATE_HOME` and
//! `XDG_CACHE_HOME`; the real binary runs on a pty over a fixture tree below them. Where `unshare -rm` is not
//! allowed the journeys are a named skip (`DC_REQUIRE_UNSHARE=1` makes it a failure). Nothing outside the
//! fixture is written: the tmpfs mounts are private to the namespace and vanish with it.

mod common;
mod explore_pty_rig;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{bin_path, repo_root, unshare_works, Fixture};
use explore_pty_rig::{Pty, LEAVE_ALT};

const SCAN_DONE: &str = "SCAN D";

fn tree(f: &Fixture, dirs: u32) -> PathBuf {
    let code = f.home().join("code");
    for i in 0..dirs {
        let d = code.join(format!("a{}", i / 50)).join(format!("b{i}"));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("f"), vec![b'x'; 3000]).unwrap();
    }
    fs::canonicalize(code).unwrap()
}

/// `explore` over `root` inside the rehearsal. When the command ends it lists everything left in the two
/// filesystems into `<fixture>/leftovers` (the filesystems exist only in the namespace).
fn rehearse(f: &Fixture, root: &Path, env: &[(&str, &str)]) -> Pty {
    let script = repo_root().join("scripts/rehearse-full-disk.sh");
    let left = f.root().join("leftovers");
    let inner = format!(
        "'{bin}' explore '{root}'; rc=$?; \
         find \"$XDG_STATE_HOME\" \"$XDG_CACHE_HOME\" -mindepth 1 -printf '%p %s\\n' > '{left}' 2>&1; exit $rc",
        bin = bin_path().display(),
        root = root.display(),
        left = left.display()
    );
    let body = format!(
        "bash '{}' --no-reserve --cache-too -- sh -c '{}'",
        script.display(),
        inner.replace('\'', "'\\''")
    );
    let mut cmd = f.command_for("sh");
    for (k, v) in env {
        cmd.env(k, v);
    }
    Pty::spawn_script(cmd, &body, &f.root().join("pty"), 30, 100)
}

fn up(p: &mut Pty) {
    p.wait_for(SCAN_DONE);
    std::thread::sleep(Duration::from_millis(200));
    p.pump();
}

fn leftovers(f: &Fixture) -> String {
    fs::read_to_string(f.root().join("leftovers")).unwrap_or_default()
}

/// No `*.tmp` file that holds data remains. A save of `last_plan` that fails on a full disk empties its
/// `last_plan.tmp` and keeps the name (`RealPlanStore::write_once`: it holds no space and the next save truncates
/// it), so a zero-length `last_plan.tmp` is the designed leftover and the only one allowed.
fn no_temp_file(f: &Fixture) {
    let left = leftovers(f);
    assert!(
        left.contains("/fill "),
        "the listing of the two filesystems is missing: {left:?}"
    );
    let kept: Vec<&str> = left
        .lines()
        .filter(|l| l.contains(".tmp"))
        .filter(|l| !(l.contains("/last_plan.tmp ") && l.ends_with(" 0")))
        .collect();
    assert!(
        kept.is_empty(),
        "a temp file with data remains: {kept:?}\nall: {left}"
    );
}

#[test]
fn browsing_and_quitting_on_a_full_state_and_cache_disk_should_exit_0_with_the_note_after_the_restore(
) {
    if !unshare_works() {
        return;
    }
    let f = Fixture::new();
    let root = tree(&f, 400);
    let mut p = rehearse(&f, &root, &[]);
    up(&mut p);
    p.keys(&[b"j", b"l", b"h"]);
    p.send(b"q");
    assert_eq!(p.exit_code(), 0, "{}", p.tail());
    p.finish();
    p.assert_restored();
    let leave = explore_pty_rig::rfind(&p.seen, LEAVE_ALT).unwrap();
    let after = String::from_utf8_lossy(&p.seen[leave..]).into_owned();
    assert!(
        after.contains("cache not saved"),
        "the cache failure is reprinted after the restore: {after:?}"
    );
    no_temp_file(&f);
}

#[test]
fn q_during_a_scan_that_would_save_should_exit_within_a_second_and_leave_no_temp_file() {
    if !unshare_works() {
        return;
    }
    let f = Fixture::new();
    let root = tree(&f, 6000);
    let mut p = rehearse(&f, &root, &[]);
    p.wait_for("SCANNING");
    let pressed = Instant::now();
    p.send(b"q");
    // A quit while a scan runs may ask nothing; if the quit prompt shows, Enter leaves.
    let code = loop {
        p.pump();
        let text = p.screen_text();
        if let Some(at) = text.find("EXIT=") {
            let digits: String = text[at + 5..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if !digits.is_empty() {
                break digits.parse::<i32>().unwrap();
            }
        }
        assert!(
            pressed.elapsed() < Duration::from_secs(20),
            "did not exit: {}",
            p.tail()
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(code, 0, "{}", p.tail());
    assert!(
        pressed.elapsed() < Duration::from_secs(1),
        "q to exit took {:?}",
        pressed.elapsed()
    );
    p.finish();
    p.assert_restored();
    no_temp_file(&f);
}

/// Marks a row, plans (`p`) and sends the signal while the 5 s planner call is running.
fn signal_during_planner(name: &str, send: impl Fn(&mut Pty), want: i32) {
    if !unshare_works() {
        return;
    }
    let f = Fixture::new();
    let root = tree(&f, 400);
    let mut p = rehearse(&f, &root, &[("DC_EXPLORE_PLAN_DELAY_MS", "5000")]);
    up(&mut p);
    p.keys(&[b"j", b" ", b"p"]);
    p.wait_for("PLANNING");
    let sent = Instant::now();
    send(&mut p);
    assert_eq!(p.exit_code(), want, "{name}: {}", p.tail());
    assert!(
        sent.elapsed() < Duration::from_secs(4),
        "{name}: the signal waited for the planner ({:?})",
        sent.elapsed()
    );
    p.finish();
    p.assert_restored();
    no_temp_file(&f);
}

#[test]
fn sigterm_during_a_five_second_planner_call_should_restore_and_exit_143() {
    signal_during_planner("sigterm", |p| p.kill("TERM"), 143);
}

#[test]
fn ctrl_c_during_a_five_second_planner_call_should_restore_and_exit_130() {
    signal_during_planner("ctrl-c", |p| p.send(&[0x03]), 130);
}

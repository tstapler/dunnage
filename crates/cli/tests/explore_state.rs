#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.5.4, Task 6.5.4c: the spawned `explore` binary. Its text and JSON stdout equal the library's, a
//! signal mid-scan ends it with the table's exit code, and a run writes nothing but the explorer cache, the
//! first-run reserve files and, after a preview, `last_plan` (UX-T32, UX-T33, N-8).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::Fixture;
use disk_cleanup_adapters::explore::cache_paint::CacheMode;
use disk_cleanup_cli::explore::json::run_json;
use disk_cleanup_cli::explore::text::run_text;
use disk_cleanup_cli::wiring::explore::{build_text_env, Settings};
use disk_cleanup_core::explore::rows::Sort;
use disk_cleanup_core::explore::testing::FakeTermSignals;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::Dirs;

/// `<fixture>/home/code/{app,lib}` with a few files.
fn tree(f: &Fixture) -> PathBuf {
    let code = f.home().join("code");
    for (dir, files) in [("app/src", 3), ("app/docs", 2), ("lib", 4)] {
        fs::create_dir_all(code.join(dir)).unwrap();
        for i in 0..files {
            fs::write(
                code.join(dir).join(format!("f{i}")),
                vec![b'x'; 3000 * (i + 1)],
            )
            .unwrap();
        }
    }
    fs::canonicalize(code).unwrap()
}

fn dirs_of(f: &Fixture) -> Dirs {
    Dirs {
        home: f.home(),
        state: f.xdg("state"),
        cache: f.xdg("cache"),
        config: f.xdg("config"),
    }
}

fn settings(root: &Path, cache: CacheMode) -> Settings {
    Settings {
        display_roots: vec![root.to_path_buf()],
        jobs: 2,
        sort: Sort::default(),
        cache,
        max_seconds: None,
        tty: false,
    }
}

/// The library's view of the same tree, run in this process over a fake signal source.
fn library_env(
    f: &Fixture,
    root: &Path,
    top: usize,
) -> disk_cleanup_cli::explore::text::ExploreEnv {
    let dirs = dirs_of(f);
    let roots = RootSet::resolve(
        &dirs,
        &[],
        Some(&[root.to_path_buf()]),
        &f.home(),
        &disk_cleanup_cli::wiring::dirs::real_canon,
    )
    .unwrap();
    let mut env = build_text_env(
        &dirs,
        &roots,
        &settings(root, CacheMode::Off),
        Arc::new(FakeTermSignals::never()),
    );
    env.top = top;
    env.width = 80;
    env
}

/// Drops what legitimately differs between two runs: the scan time and the live free-space figures.
fn mask_text(s: &str) -> String {
    s.split("\n\n")
        .enumerate()
        .filter(|(i, _)| *i != 1)
        .map(|(i, p)| {
            if i == 0 {
                p.chars()
                    .filter(|c| !c.is_ascii_digit())
                    .collect::<String>()
            } else {
                p.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[test]
fn the_binary_text_view_on_a_pipe_should_equal_run_text_for_the_same_tree() {
    let f = Fixture::new();
    let root = tree(&f);
    let out = f
        .fixture_cmd()
        .args(["explore", "--text", "--no-cache", "--top", "6"])
        .arg(&root)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let (mut lib_out, mut lib_err) = (Vec::new(), Vec::new());
    let code = run_text(&library_env(&f, &root, 6), &mut lib_out, &mut lib_err);
    assert_eq!(code, 0);
    assert_eq!(
        mask_text(&String::from_utf8(out.stdout).unwrap()),
        mask_text(&String::from_utf8(lib_out).unwrap())
    );
}

#[test]
fn the_binary_json_should_equal_run_json_for_the_same_tree() {
    let f = Fixture::new();
    let root = tree(&f);
    let out = f
        .fixture_cmd()
        .args(["explore", "--json", "--no-cache", "--top", "6"])
        .arg(&root)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let mut lib_out = Vec::new();
    let mut env = library_env(&f, &root, 6);
    env.depth = 1;
    assert_eq!(run_json(&env, &mut lib_out, &mut Vec::new()), 0);
    let drop_live = |bytes: &[u8]| {
        let mut v: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        v.as_object_mut().unwrap().remove("filesystem");
        v
    };
    assert_eq!(drop_live(&out.stdout), drop_live(&lib_out));
}

#[test]
fn a_text_run_should_write_the_explorer_cache_and_no_state_at_all() {
    let f = Fixture::new();
    let root = tree(&f);
    let out = f
        .fixture_cmd()
        .args(["explore", "--text"])
        .arg(&root)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(f.xdg("cache").join("disk-cleanup/explorer.cache").is_file());
    assert!(!f.xdg("cache").join("disk-cleanup/walker.cache").exists());
    assert!(
        !f.xdg("state").join("disk-cleanup").exists(),
        "a text run created state: {:?}",
        fs::read_dir(f.xdg("state")).unwrap().collect::<Vec<_>>()
    );
}

#[test]
fn a_scan_stopped_by_max_seconds_should_say_partial_and_still_exit_0() {
    // 0 s is a deadline that has already passed: a tiny tree cannot be made to outlast 1 s on every host.
    let f = Fixture::new();
    let root = tree(&f);
    let out = f
        .fixture_cmd()
        .args(["explore", "--text", "--no-cache", "--max-seconds", "0"])
        .arg(&root)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("PARTIAL SCAN (hit --max-seconds 0)"),
        "{text}"
    );
}

/// A tree big enough that a cold scan takes longer than the test needs to deliver a signal.
fn big_tree(f: &Fixture) -> PathBuf {
    let root = f.home().join("big");
    for a in 0..40 {
        for b in 0..100 {
            fs::create_dir_all(root.join(format!("d{a}/e{b}"))).unwrap();
        }
    }
    fs::canonicalize(root).unwrap()
}

fn signal_mask(pid: u32) -> u64 {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("SigCgt:"))
        .and_then(|h| u64::from_str_radix(h.trim(), 16).ok())
        .unwrap_or(0)
}

fn kill(sig: &str, pid: u32) {
    assert!(Command::new("kill")
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .status()
        .unwrap()
        .success());
}

/// Starts `explore --text` on the big tree, waits until its handlers are installed, then delivers `sig`
/// while the process is stopped, so the signal is pending before the scan can end.
fn signalled(sig: &str) -> (Option<i32>, String, String) {
    let f = Fixture::new();
    let root = big_tree(&f);
    let child = f
        .fixture_cmd()
        .args(["explore", "--text", "--no-cache"])
        .arg(&root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let end = Instant::now() + Duration::from_secs(20);
    // SIGINT is bit 1, SIGTERM bit 14.
    while signal_mask(pid) & 0x4002 != 0x4002 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(1));
    }
    kill("STOP", pid);
    kill(sig, pid);
    kill("CONT", pid);
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn sigint_mid_scan_should_print_the_interrupted_line_and_exit_130() {
    let (code, stdout, stderr) = signalled("INT");
    assert_eq!(code, Some(130), "{stderr}");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("INTERRUPTED during scan. Nothing was deleted; no report produced."),
        "{stderr}"
    );
}

#[test]
fn sigterm_sighup_and_sigquit_mid_scan_should_all_exit_143() {
    for sig in ["TERM", "HUP", "QUIT"] {
        let (code, stdout, stderr) = signalled(sig);
        assert_eq!(code, Some(143), "{sig}: {stderr}");
        assert_eq!(stdout, "", "{sig}");
    }
}

// ----- the interactive screen on a pty -----

struct Pty {
    session: expectrl::Session,
    seen: Vec<u8>,
}

impl Pty {
    /// `explore <args>` under the fixture environment, on a 100x30 pty with `TERM=xterm`.
    fn spawn(f: &Fixture, args: &[&Path]) -> Pty {
        let mut cmd = f.command_for("sh");
        cmd.env("TERM", "xterm");
        let quoted: Vec<String> = args.iter().map(|a| format!("'{}'", a.display())).collect();
        cmd.arg("-c").arg(format!(
            "stty rows 30 cols 100; '{}' explore {}; echo EXIT=$?",
            env!("CARGO_BIN_EXE_disk-cleanup"),
            quoted.join(" ")
        ));
        let mut session = expectrl::Session::spawn(cmd).unwrap();
        session.set_expect_timeout(Some(Duration::from_secs(20)));
        Pty {
            session,
            seen: Vec::new(),
        }
    }

    fn pump(&mut self) {
        let mut buf = [0u8; 8192];
        while let Ok(n) = self.session.try_read(&mut buf) {
            if n == 0 {
                break;
            }
            self.seen.extend_from_slice(&buf[..n]);
        }
    }

    /// The transcript as a person reads the screen: a cursor move is a space, other escapes vanish.
    fn screen_text(&self) -> String {
        let raw = String::from_utf8_lossy(&self.seen).into_owned();
        let mut out = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\u{1b}' {
                out.push(c);
                continue;
            }
            if chars.peek() == Some(&'[') {
                chars.next();
                let mut last = ' ';
                for n in chars.by_ref() {
                    last = n;
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
                if last == 'H' {
                    out.push(' ');
                }
            }
        }
        out
    }

    /// Waits until `needle` is on the screen text (cursor moves read as spaces); returns where it starts.
    fn wait_for(&mut self, needle: &str) -> usize {
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            self.pump();
            if let Some(at) = self.screen_text().find(needle) {
                return at;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "never saw {needle:?}; screen text: {:?}",
            self.screen_text()
        );
    }

    fn send(&mut self, bytes: &[u8]) {
        self.session.send(bytes).unwrap();
    }

    fn exit_code(&mut self) -> i32 {
        let at = self.wait_for("EXIT=");
        let rest = self.screen_text()[at + 5..].to_string();
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().unwrap()
    }
}

fn state_names(f: &Fixture) -> Vec<String> {
    let dir = f.xdg("state").join("disk-cleanup");
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn the_first_interactive_run_should_note_the_reserve_files_before_the_alternate_screen() {
    let f = Fixture::new();
    let root = tree(&f);
    let mut p = Pty::spawn(&f, &[&root]);
    p.wait_for("created 2 reserve files of 1 MiB");
    p.wait_for("SCAN DONE");
    let raw = String::from_utf8_lossy(&p.seen).into_owned();
    let (note, alt) = (
        raw.find("created 2 reserve files").unwrap(),
        raw.find("\x1b[?1049h").unwrap(),
    );
    assert!(
        note < alt,
        "the note must come before the alternate screen: {raw:?}"
    );
    p.send(b"q");
    assert_eq!(p.exit_code(), 0);
    let names = state_names(&f);
    assert!(
        names.iter().all(|n| n.contains("reserve")),
        "only the reserve files may exist after a browse: {names:?}"
    );
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(f.xdg("cache").join("disk-cleanup/explorer.cache").is_file());
}

#[test]
fn a_held_run_lock_should_not_stop_the_browse_and_should_show_the_in_flux_note() {
    let f = Fixture::new();
    let root = tree(&f);
    let state = f.xdg("state").join("disk-cleanup");
    fs::create_dir_all(&state).unwrap();
    let lock =
        disk_cleanup_adapters::lock::RunLock::try_acquire(&state.join("run.lock"), None).unwrap();
    let mut p = Pty::spawn(&f, &[&root]);
    p.wait_for(&format!(
        "another disk-cleanup apply is running (pid {}); sizes are in flux",
        std::process::id()
    ));
    p.send(b"q");
    assert_eq!(p.exit_code(), 0);
    drop(lock);
    assert!(!state.join("audit.jsonl").exists());
}

#[test]
fn a_preview_should_save_last_plan_and_still_create_no_lock_audit_or_outcome_file() {
    let f = Fixture::new();
    let root = tree(&f);
    let mut p = Pty::spawn(&f, &[&root]);
    p.wait_for("SCAN DONE");
    p.send(b" ");
    p.wait_for("marked ");
    p.send(b"p");
    p.wait_for("PLAN PREVIEW - nothing is deleted.");
    p.send(b"\x1b");
    p.send(b"q");
    assert_eq!(p.exit_code(), 0);
    assert_eq!(
        state_names(&f),
        [
            "audit.reserve",
            "last_plan",
            "last_plan.lock",
            "last_plan.reserve"
        ],
        "a preview adds only the plan files to the reserve files"
    );
}

#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Stamp file and break-glass terminal tests (Story 2.2.1, Task 2.2.1d; validation T-471, T-516).
//!
//! The terminal legs re-run this test binary as a child (`DC_BG_CHILD=1`) under `setsid -w` (no controlling
//! terminal) and under `script` (a pty the test creates), so nothing touches the real terminal and the child
//! writes only to stdout.

use std::io::Write;
use std::process::{Command, Stdio};

use dunnage_adapters::gate_stamp::{
    break_glass, confirm_bypass, open_controlling_tty, read_stamp, sha256_file, BreakGlassRefusal,
};
use dunnage_core::STAMP_SCHEMA;

const CHILD: &str = "DC_BG_CHILD";

/// Runs only as the child of the tests below.
#[test]
fn child_probe() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let var = std::env::var("DUNNAGE_UNGATED").ok();
    let r = break_glass(var.as_deref(), &open_controlling_tty);
    let (word, code) = match r {
        Ok(bg) => (
            format!("OK tty_confirmed={} reason={}", bg.tty_confirmed, bg.reason),
            0,
        ),
        Err(BreakGlassRefusal::NoTerminal) => ("NO_TTY".to_string(), 64),
        Err(BreakGlassRefusal::NotConfirmed) => ("NOT_CONFIRMED".to_string(), 64),
        Err(BreakGlassRefusal::NotRequested) => ("NOT_REQUESTED".to_string(), 64),
    };
    println!("\nRESULT:{word}");
    std::io::stdout().flush().unwrap();
    std::process::exit(code);
}

fn child_cmd() -> Command {
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args(["--exact", "child_probe", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("DUNNAGE_UNGATED", "rebuilt locally for Gate B");
    c
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn result_line(out: &[u8]) -> String {
    String::from_utf8_lossy(out)
        .lines()
        .find_map(|l| l.trim().strip_prefix("RESULT:").map(str::to_string))
        .unwrap_or_default()
}

fn under_pty(typed: &str) -> (Option<i32>, String) {
    let exe = std::env::current_exe().unwrap();
    let inner = format!(
        "{} --exact child_probe --nocapture --test-threads=1",
        exe.display()
    );
    let mut c = Command::new("script");
    c.args(["-qec", &inner, "/dev/null"])
        .env(CHILD, "1")
        .env("DUNNAGE_UNGATED", "rebuilt locally for Gate B")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = c.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(typed.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    drop(stdin);
    (out.status.code(), result_line(&out.stdout))
}

#[test]
fn break_glass_should_exit_64_without_a_controlling_terminal_and_proceed_only_with_BYPASS_typed_on_the_pty(
) {
    if !have("setsid") {
        eprintln!("skip: setsid not installed");
        return;
    }
    let out = Command::new("setsid")
        .arg("-w")
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "child_probe", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("DUNNAGE_UNGATED", "rebuilt locally for Gate B")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(result_line(&out.stdout), "NO_TTY");
    assert_eq!(out.status.code(), Some(64));

    // The same child with no variable never reaches the terminal.
    let out = child_cmd()
        .env_remove("DUNNAGE_UNGATED")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(result_line(&out.stdout), "NOT_REQUESTED");

    if !have("script") {
        eprintln!("skip: pty legs, script not installed");
        return;
    }
    let (code, line) = under_pty("BYPASS\n");
    assert_eq!(
        line,
        "OK tty_confirmed=true reason=rebuilt locally for Gate B"
    );
    assert_eq!(code, Some(0));
    for wrong in ["bypass\n", "yes\n", "BYPASS now\n", "\n", "\u{4}"] {
        let (code, line) = under_pty(wrong);
        assert_eq!(line, "NOT_CONFIRMED", "typed {wrong:?}");
        assert_eq!(code, Some(64), "typed {wrong:?}");
    }
}

#[test]
fn confirm_bypass_should_accept_only_the_exact_word_on_one_line() {
    for (typed, want) in [
        ("BYPASS\n", true),
        ("BYPASS\r\n", true),
        ("BYPASS", true),
        (" BYPASS\n", false),
        ("bypass\n", false),
        ("", false),
    ] {
        let mut sink = Vec::new();
        let mut io = Duplex {
            r: std::io::Cursor::new(typed.as_bytes().to_vec()),
            w: &mut sink,
        };
        assert_eq!(confirm_bypass("r", &mut io).unwrap(), want, "{typed:?}");
        assert!(String::from_utf8(sink).unwrap().contains("Type BYPASS"));
    }
}

struct Duplex<'a> {
    r: std::io::Cursor<Vec<u8>>,
    w: &'a mut Vec<u8>,
}
impl std::io::Read for Duplex<'_> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.r.read(b)
    }
}
impl std::io::BufRead for Duplex<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.r.fill_buf()
    }
    fn consume(&mut self, n: usize) {
        self.r.consume(n);
    }
}
impl Write for Duplex<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.w.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn read_stamp_and_sha256_file_should_read_what_gate_sh_writes() {
    let d = tempfile::tempdir().unwrap();
    let bin = d.path().join("bin");
    std::fs::write(&bin, b"abc").unwrap();
    assert_eq!(
        sha256_file(&bin).unwrap(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let p = d.path().join("gate-stamp.json");
    assert!(read_stamp(&p).is_none());
    std::fs::write(&p, "not json").unwrap();
    assert!(read_stamp(&p).is_none());
    std::fs::write(
        &p,
        r#"{"schema":1,"gate":"B","commit":"abc","binary_sha256":"def"}"#,
    )
    .unwrap();
    let s = read_stamp(&p).unwrap();
    assert_eq!((s.schema, s.gate.as_str()), (STAMP_SCHEMA, "B"));
    std::fs::write(&p, r#"{"schema":7,"x":1}"#).unwrap();
    assert_eq!(read_stamp(&p).unwrap().schema, 7);
}

#[test]
fn stamp_refusal_should_offer_both_the_rebuild_and_the_audited_break_glass() {
    let t = dunnage_core::StampRefusal::Absent.text();
    assert!(
        t.contains("If the disk has room for a build, run 'make gate"),
        "{t}"
    );
    assert!(
        t.contains("DUNNAGE_UNGATED=<reason>") && t.contains("type BYPASS"),
        "{t}"
    );
}

#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The probe under a real pty (Task 6.5.2c; UX-T23): after every way the explorer can end the transcript
//! shows the alternate screen left and the cursor shown, `stty -a` equals the state saved before, and no
//! raw ESC from the session is left on the normal screen.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use expectrl::Session;

const LEAVE_ALT: &[u8] = b"\x1b[?1049l";
const ENTER_ALT: &[u8] = b"\x1b[?1049h";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const RESET: &[u8] = b"\x1b[0m";

fn probe() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let deps = exe.parent().unwrap();
    deps.parent().unwrap().join("examples").join("guard_probe")
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn count(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

struct Pty {
    session: Option<Session>,
    seen: Vec<u8>,
    dir: PathBuf,
}

impl Pty {
    fn spawn(name: &str, mode: &str) -> Pty {
        let dir = std::env::temp_dir().join(format!("tui-pty-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let script = format!(
            "stty -a > '{d}/before'; '{p}' '{d}' {mode}; echo SHELL-AFTER; stty -a > '{d}/after'",
            d = dir.display(),
            p = probe().display()
        );
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        let mut session = Session::spawn(cmd).unwrap();
        session.set_expect_timeout(Some(Duration::from_secs(20)));
        Pty {
            session: Some(session),
            seen: Vec::new(),
            dir,
        }
    }

    fn pump(&mut self) {
        let mut buf = [0u8; 8192];
        while let Ok(n) = self
            .session
            .as_mut()
            .map_or(Ok(0), |s| s.try_read(&mut buf))
        {
            if n == 0 {
                break;
            }
            self.seen.extend_from_slice(&buf[..n]);
        }
    }

    fn wait_for(&mut self, needle: &[u8], from: usize) -> bool {
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            self.pump();
            if find(&self.seen[from..], needle).is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn wait_file(&mut self, name: &str) -> String {
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            self.pump();
            if let Ok(t) = fs::read_to_string(self.dir.join(name)) {
                if !t.is_empty() {
                    return t;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("no {name} file; end of transcript: {:?}", self.tail());
    }

    fn tail(&self) -> String {
        let t = &self.seen;
        String::from_utf8_lossy(&t[t.len().saturating_sub(400)..]).into_owned()
    }

    /// Waits for the stop (the restore before it) and for the re-entry. Some shells continue a stopped
    /// child on their own; if the screen has not come back, SIGCONT is sent here.
    fn suspend_and_resume(&mut self, from: usize) {
        assert!(
            self.wait_for(LEAVE_ALT, from),
            "restored before the stop: {}",
            self.tail()
        );
        let left = from + find(&self.seen[from..], LEAVE_ALT).unwrap() + LEAVE_ALT.len();
        std::thread::sleep(Duration::from_millis(300));
        self.pump();
        if find(&self.seen[left..], ENTER_ALT).is_none() {
            self.kill("CONT");
        }
        assert!(
            self.wait_for(ENTER_ALT, left),
            "re-entered after the stop: {}",
            self.tail()
        );
        assert!(
            self.wait_for(b"SCANNING", left),
            "redrawn after the stop: {}",
            self.tail()
        );
    }

    fn up(&mut self) -> usize {
        assert!(
            self.wait_for(b"SCANNING", 0),
            "no first frame: {:?}",
            self.tail()
        );
        self.seen.len()
    }

    fn kill(&self, sig: &str) {
        let pid = fs::read_to_string(self.dir.join("pid")).unwrap();
        let st = Command::new("kill")
            .arg(format!("-{sig}"))
            .arg(pid.trim())
            .status()
            .unwrap();
        assert!(st.success());
    }

    fn send(&mut self, bytes: &[u8]) {
        self.session.as_mut().unwrap().send(bytes).unwrap();
    }

    /// Reads until the shell has run the trailing `stty -a`.
    fn finish(&mut self) {
        assert!(self.wait_for(b"SHELL-AFTER", 0), "{:?}", self.tail());
        let end = Instant::now() + Duration::from_secs(10);
        while !self.dir.join("after").exists() && Instant::now() < end {
            self.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
        self.pump();
    }

    /// The assertions every ending shares.
    fn assert_restored(&self) {
        let t = &self.seen;
        assert!(find(t, ENTER_ALT).is_some());
        let leave = t
            .windows(LEAVE_ALT.len())
            .rposition(|w| w == LEAVE_ALT)
            .expect("the alternate screen was left");
        assert!(find(t, SHOW_CURSOR).is_some(), "the cursor was shown");
        let before = fs::read_to_string(self.dir.join("before")).unwrap();
        let after = fs::read_to_string(self.dir.join("after")).unwrap();
        assert_eq!(before, after, "stty -a differs after the session");
        // Past the last restore only the reset the guard writes (ESC[0m) may follow; no other raw ESC.
        let mut tail = t[leave + LEAVE_ALT.len()..].to_vec();
        while let Some(i) = find(&tail, RESET) {
            tail.drain(i..i + RESET.len());
        }
        while let Some(i) = find(&tail, SHOW_CURSOR) {
            tail.drain(i..i + SHOW_CURSOR.len());
        }
        assert!(
            !tail.contains(&0x1b),
            "raw ESC after the restore: {:?}",
            String::from_utf8_lossy(&tail)
        );
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn exit_of(p: &mut Pty) -> i32 {
    p.wait_file("exit").trim().parse().unwrap()
}

fn signal_case(name: &str, sig: &str, want: i32) {
    let mut p = Pty::spawn(name, "hold");
    p.up();
    p.kill(sig);
    assert_eq!(exit_of(&mut p), want, "{name}");
    p.finish();
    p.assert_restored();
}

#[test]
fn sigterm_restores_and_exits_143() {
    signal_case("term", "TERM", 143);
}

#[test]
fn sighup_restores_and_exits_143() {
    signal_case("hup", "HUP", 143);
}

#[test]
fn sigquit_restores_and_exits_143() {
    signal_case("quit", "QUIT", 143);
}

#[test]
fn an_external_sigint_restores_and_exits_130() {
    signal_case("int", "INT", 130);
}

#[test]
fn the_ctrl_c_key_restores_and_exits_130() {
    let mut p = Pty::spawn("ctrlc", "hold");
    p.up();
    p.send(&[0x03]);
    assert_eq!(exit_of(&mut p), 130);
    p.finish();
    p.assert_restored();
}

#[test]
fn q_restores_prints_the_closing_line_after_the_restore_and_exits_0() {
    let mut p = Pty::spawn("q", "hold");
    p.up();
    p.send(b"q");
    assert_eq!(exit_of(&mut p), 0);
    p.finish();
    p.assert_restored();
    let leave = p
        .seen
        .windows(LEAVE_ALT.len())
        .rposition(|w| w == LEAVE_ALT)
        .unwrap();
    let closing = find(&p.seen, b"Closed while scanning").expect("the closing line");
    assert!(
        closing > leave,
        "the closing line is printed after the restore"
    );
}

#[test]
fn an_external_sigtstp_restores_stops_and_sigcont_reenters_and_redraws() {
    let mut p = Pty::spawn("tstp", "hold");
    let from = p.up();
    p.kill("TSTP");
    p.suspend_and_resume(from);
    p.send(b"q");
    assert_eq!(exit_of(&mut p), 0);
    p.finish();
    p.assert_restored();
    assert_eq!(count(&p.seen, ENTER_ALT), 2);
}

#[test]
fn ctrl_z_then_continue_restores_stops_reenters_and_redraws() {
    let mut p = Pty::spawn("ctrlz", "hold");
    let from = p.up();
    p.send(&[0x1a]);
    p.suspend_and_resume(from);
    p.send(b"q");
    assert_eq!(exit_of(&mut p), 0);
    p.finish();
    p.assert_restored();
}

#[test]
fn a_panic_on_the_ui_thread_restores_first_and_then_prints_the_message() {
    let mut p = Pty::spawn("panic", "panic");
    p.up();
    p.send(b"j ");
    p.finish();
    p.assert_restored();
    let leave = p
        .seen
        .windows(LEAVE_ALT.len())
        .rposition(|w| w == LEAVE_ALT)
        .unwrap();
    let msg = find(&p.seen, b"forced panic in mark_check").expect("the panic message is printed");
    assert!(msg > leave, "the message follows the restore");
}

#[test]
fn a_closed_pty_master_fails_the_restore_writes_with_eio_and_the_signal_code_still_wins() {
    let mut p = Pty::spawn("hangup", "hold");
    p.up();
    let dir = p.dir.clone();
    // Dropping the session closes the master: the kernel sends SIGHUP and every later write fails with EIO.
    drop(p.session.take());
    let end = Instant::now() + Duration::from_secs(20);
    let mut code = None;
    while Instant::now() < end {
        if let Ok(t) = fs::read_to_string(dir.join("exit")) {
            if !t.is_empty() {
                code = Some(t.trim().parse::<i32>().unwrap());
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        code,
        Some(143),
        "SIGHUP, not 74 for the failed restore write"
    );
}

#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.3.3 Task 6.3.3a (validation X-089): `RealTermSignals` in a helper process, `dc-sigprobe`, which
//! this test signals from outside. The helper prints what it sees; nothing in this process installs a handler.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use rustix::process::{kill_process, waitpid, Pid, Signal, WaitOptions};

const WAIT: Duration = Duration::from_secs(10);

struct Probe {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Probe {
    fn start() -> Probe {
        Probe::start_with(&[])
    }

    fn start_with(args: &[&str]) -> Probe {
        // Its own process group, so a stop signal is acted on even when the harness has no tty.
        let mut child = Command::new(env!("CARGO_BIN_EXE_dc-sigprobe"))
            .args(args)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(l).is_err() {
                    return;
                }
            }
        });
        let mut p = Probe {
            child,
            stdin,
            lines,
        };
        assert_eq!(p.next_line(), "ready");
        p
    }

    fn pid(&self) -> Pid {
        Pid::from_child(&self.child)
    }

    fn next_line(&mut self) -> String {
        self.lines
            .recv_timeout(WAIT)
            .expect("the helper said nothing")
    }

    fn quiet_for(&mut self, d: Duration) -> bool {
        self.lines.recv_timeout(d).is_err()
    }

    fn send(&self, sig: Signal) {
        kill_process(self.pid(), sig).unwrap();
    }

    fn command(&mut self, cmd: &str) {
        writeln!(self.stdin, "{cmd}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn finish(mut self) {
        self.command("quit");
        let status = self.child.wait().unwrap();
        assert!(status.success(), "{status:?}");
    }
}

fn signal_is_reported_as(sig: Signal, number: i32) {
    let mut p = Probe::start();
    p.send(sig);
    assert_eq!(p.next_line(), format!("interrupted {number}"));
    p.finish();
}

#[test]
fn term_signals_should_report_15_1_2_and_3_as_data_without_killing_the_helper() {
    signal_is_reported_as(Signal::TERM, 15);
    signal_is_reported_as(Signal::HUP, 1);
    signal_is_reported_as(Signal::INT, 2);
    signal_is_reported_as(Signal::QUIT, 3);
}

#[test]
fn the_first_signal_should_stay_the_answer() {
    let mut p = Probe::start();
    p.send(Signal::TERM);
    assert_eq!(p.next_line(), "interrupted 15");
    p.send(Signal::INT);
    assert!(
        p.quiet_for(Duration::from_millis(300)),
        "a later SIGINT must not replace the first signal"
    );
    p.finish();
}

#[test]
fn an_external_sigtstp_should_set_the_suspend_flag_once() {
    let mut p = Probe::start();
    p.send(Signal::TSTP);
    assert_eq!(p.next_line(), "tstp");
    assert!(
        p.quiet_for(Duration::from_millis(300)),
        "the flag is read and cleared, so it reports once"
    );
    p.finish();
}

fn wait_for(p: &Probe, options: WaitOptions) -> rustix::process::WaitStatus {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some((_, status)) = waitpid(Some(p.pid()), options | WaitOptions::NOHANG).unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "the helper never changed state");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn suspend_should_stop_the_process_and_return_on_sigcont_and_work_a_second_time() {
    let mut p = Probe::start();
    for round in 1..=2 {
        p.command("suspend");
        let status = wait_for(&p, WaitOptions::UNTRACED);
        assert!(status.stopped(), "round {round}: stopped, got {status:?}");
        assert_eq!(status.stopping_signal(), Some(Signal::TSTP.as_raw()));
        p.send(Signal::CONT);
        let status = wait_for(&p, WaitOptions::CONTINUED);
        assert!(status.continued(), "round {round}: {status:?}");
        assert_eq!(p.next_line(), "resumed");
    }
    // still alive and still reporting signals as data
    p.send(Signal::TERM);
    assert_eq!(p.next_line(), "interrupted 15");
    p.finish();
}

/// `SigIgn` in `/proc/self/status` is a hex mask; bit 0 is SIGHUP.
fn sighup_is_ignored_here() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("SigIgn:").map(|v| v.trim().to_string()))
        })
        .and_then(|v| u64::from_str_radix(&v, 16).ok())
        .is_some_and(|m| m & 1 == 1)
}

#[test]
fn the_run_guard_signal_handlers_should_be_unchanged_and_sighup_should_keep_its_default_during_an_apply(
) {
    if sighup_is_ignored_here() {
        // A child inherits an ignored SIGHUP and no safe code can reset it (a shell cannot either), so the
        // default-disposition half proves nothing under nohup. The SIGINT half below still runs.
        eprintln!("SKIPPED the SIGHUP half: this test run inherited SIGHUP as ignored (nohup or a background launch)");
    } else {
        let p = Probe::start_with(&["apply"]);
        p.send(Signal::HUP);
        let status = wait_for(&p, WaitOptions::empty());
        assert_eq!(
            status.terminating_signal(),
            Some(Signal::HUP.as_raw()),
            "SIGHUP must still kill an apply run: {status:?}"
        );
    }
    // SIGINT in apply mode is still the run's own handler (the abort flag), not the explorer's
    let mut p = Probe::start_with(&["apply"]);
    p.send(Signal::INT);
    assert!(p.quiet_for(Duration::from_millis(300)));
    assert!(
        p.child.try_wait().unwrap().is_none(),
        "the apply handler absorbs the first SIGINT"
    );
    p.finish();
}

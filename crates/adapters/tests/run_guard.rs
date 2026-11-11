//! Story 2.2.2: run lock, signal handling and closed-stdout behaviour. Signal and cross-process cases re-run
//! this test binary as a child (`role_child`, selected by `DC_TEST_ROLE`) so a real handler can be installed
//! without touching the harness process; everything lives in tempdirs and on fixture scripts.
#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustix::process::{getpgid, kill_process, Pid, Signal};

use disk_cleanup_adapters::lock::{LockError, RealRunGuard, RunLock};
use disk_cleanup_adapters::signals::{
    self, ChildInFlight, Machine, Mode, Reaction, SignalState, SwallowBrokenPipe, SCAN_INTERRUPTED,
};
use disk_cleanup_adapters::RealCommandRunner;
use disk_cleanup_core::guard::{decide_path_common, Facts, GuardDecision};
use disk_cleanup_core::ports::{CommandRunner, RunGuard};
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::{ApplyToken, Dirs, FatalError, Identity, Kind, RootDev, WorktreeRemoval};

/// A fork in another test thread briefly shares this process's open lock files until it execs, which can
/// make a just-released in-process lock look held; in-process lock tests and spawns take turns.
static FORK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn fork_turn() -> std::sync::MutexGuard<'static, ()> {
    FORK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

const WT: &str = "/h/code/r/.claude/worktrees/agent-1";

// ------------------------------------------------------------------ child side

fn mark(s: &str) {
    println!("DC:{s}");
    let _ = std::io::stdout().flush();
}

fn worktree_removal() -> WorktreeRemoval {
    let dirs = Dirs {
        home: "/h".into(),
        state: "/h/.local/state".into(),
        cache: "/h/.cache".into(),
        config: "/h/.config".into(),
    };
    let canon = |p: &Path| -> std::io::Result<(PathBuf, Identity)> {
        Ok((
            p.to_path_buf(),
            Identity {
                dev: 28,
                ino: p.as_os_str().len() as u64,
            },
        ))
    };
    let roots =
        RootSet::resolve(&dirs, &["/h/code".into()], None, Path::new("/h"), &canon).unwrap();
    let facts = Facts {
        kind: Kind::Worktree,
        path: WT.into(),
        ident: Identity { dev: 28, ino: 7 },
        is_symlink: false,
        root_dev: RootDev(28),
        is_mount_point: false,
    };
    match decide_path_common(&facts, &roots) {
        GuardDecision::Allow(a) => WorktreeRemoval::new(a, "/h/code/r".into()).unwrap(),
        GuardDecision::Skip(r) => panic!("{r:?}"),
    }
}

fn install(mode: Mode) -> Arc<SignalState> {
    let st = Arc::new(SignalState::new());
    signals::install(mode, Arc::clone(&st), Box::new(std::io::stderr())).unwrap();
    st
}

#[test]
fn role_child() {
    let Ok(role) = std::env::var("DC_TEST_ROLE") else {
        return;
    };
    // watchdog: a child orphaned by a failed parent exits by itself
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(30));
        std::process::exit(99);
    });
    let arg = std::env::var("DC_TEST_ARG").unwrap_or_default();
    match role.as_str() {
        "hold" => {
            let _lock = RunLock::try_acquire(Path::new(&arg), None).unwrap();
            mark("ready");
            std::thread::sleep(Duration::from_secs(25));
        }
        "items" => {
            let st = install(Mode::Apply);
            mark("ready");
            let mut attempted = 0;
            for n in 1..=7 {
                if st.abort.is_set() {
                    break;
                }
                attempted = n;
                mark(&format!("item {n}"));
                // item 3 stays in flight until the signal has arrived, whatever the machine's load
                let until = Instant::now() + Duration::from_secs(10);
                while n == 3 && !st.abort.is_set() && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            match st.interrupted() {
                Some(sig) => {
                    mark(&format!("interrupted sig={sig} attempted={attempted}"));
                    std::process::exit(128 + sig);
                }
                None => mark("complete"),
            }
            std::process::exit(0);
        }
        "stuck" => {
            let _st = install(Mode::Apply);
            mark("ready");
            std::thread::sleep(Duration::from_secs(25));
        }
        "plan" => {
            signals::install(
                Mode::Plan,
                Arc::new(SignalState::new()),
                Box::new(std::io::stderr()),
            )
            .unwrap();
            mark("ready");
            std::thread::sleep(Duration::from_secs(25));
            mark("scan finished");
        }
        "worktree" => {
            let st = install(Mode::Apply);
            let r = RealCommandRunner::new();
            mark("ready");
            let out = r.remove_worktree(&worktree_removal(), &ApplyToken::for_tests());
            mark(&format!("git returned ok={}", out.is_ok()));
            let sig = st.interrupted().unwrap_or(0);
            mark(&format!("stopped sig={sig}"));
            std::process::exit(if sig == 0 { 0 } else { 128 + sig });
        }
        "pipe" => {
            signals::default_sigpipe().unwrap();
            signals::ignore_sigpipe().unwrap();
            let mut out = SwallowBrokenPipe::new(std::io::stdout());
            mark("ready");
            // the parent closes its read end after `ready`, then sends the go-ahead on stdin
            let mut go = String::new();
            std::io::stdin().read_line(&mut go).unwrap();
            let mut failed = 0;
            for i in 0..2000 {
                if writeln!(out, "line {i} of the closed pipe").is_err() {
                    failed += 1;
                }
            }
            if out.flush().is_err() {
                failed += 1;
            }
            eprintln!("DC:pipe failed={failed} closed={}", out.is_closed());
            std::process::exit(0);
        }
        other => panic!("unknown role {other}"),
    }
}

// ------------------------------------------------------------------ parent side

struct Spawned {
    child: Child,
    lines: Receiver<String>,
    err: Receiver<String>,
}

fn spawn_role(role: &str, arg: &str, path_prefix: Option<&Path>) -> Spawned {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "role_child", "--nocapture", "--test-threads=1"])
        .env("DC_TEST_ROLE", role)
        .env("DC_TEST_ARG", arg)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(p) = path_prefix {
        let old = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{old}", p.display()));
    }
    let mut child = {
        let _turn = fork_turn();
        cmd.spawn().unwrap()
    };
    let (tx, rx) = channel();
    let out = child.stdout.take().unwrap();
    let hang_up_after_ready = role == "pipe";
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            // the harness prints `test role_child ... ` before the first marker on the same line
            if let Some(at) = l.find("DC:") {
                let _ = tx.send(l[at + 3..].to_string());
                if hang_up_after_ready {
                    return; // drops the read end: the child's later stdout writes hit EPIPE
                }
            }
        }
    });
    let (etx, erx) = channel();
    let err = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for l in BufReader::new(err).lines().map_while(Result::ok) {
            let _ = etx.send(l);
        }
    });
    Spawned {
        child,
        lines: rx,
        err: erx,
    }
}

impl Spawned {
    fn expect(&self, prefix: &str) -> String {
        loop {
            let l = self
                .lines
                .recv_timeout(Duration::from_secs(20))
                .unwrap_or_else(|_| panic!("no `{prefix}` marker from the child"));
            if l.starts_with(prefix) {
                return l;
            }
        }
    }

    fn pid(&self) -> Pid {
        Pid::from_child(&self.child)
    }

    fn signal(&self, sig: i32) {
        kill_process(
            self.pid(),
            if sig == libc::SIGINT {
                Signal::INT
            } else {
                Signal::TERM
            },
        )
        .unwrap();
    }

    fn finish(mut self) -> (i32, Vec<String>, Vec<String>) {
        let status = self.child.wait().unwrap();
        let code = status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap());
        // the reader threads end at EOF, which the child's exit brings; drain until they hang up
        let drain = |rx: &Receiver<String>| {
            let mut v = Vec::new();
            while let Ok(l) = rx.recv_timeout(Duration::from_secs(5)) {
                v.push(l);
            }
            v
        };
        let out = drain(&self.lines);
        (code, out, drain(&self.err))
    }
}

#[test]
fn run_lock_should_refuse_a_second_holder_with_its_pid_and_release_on_drop() {
    let _turn = fork_turn();
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("run.lock");
    let first = RunLock::try_acquire(&p, None).unwrap();
    assert_eq!(
        std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o600
    );
    match RunLock::try_acquire(&p, None) {
        Err(LockError::Held { pid }) => assert_eq!(pid, std::process::id()),
        other => panic!("{other:?}"),
    }
    drop(first);
    assert!(RunLock::try_acquire(&p, None).is_ok());
}

#[test]
fn acquire_should_give_LockHeld_with_the_text_of_the_catalogue_When_another_process_holds_run_lock()
{
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("run.lock");
    let holder = spawn_role("hold", p.to_str().unwrap(), None);
    holder.expect("ready");
    let _turn = fork_turn();
    let pid = holder.child.id();
    let guard = RealRunGuard::new(p.clone(), None, Arc::new(SignalState::new()));
    let err = guard.acquire().unwrap_err();
    assert_eq!(err, FatalError::LockHeld { pid });
    assert_eq!(err.exit_code(), 2);
    assert_eq!(
        err.text(),
        format!("error: another disk-cleanup run is active (pid {pid}). Nothing was deleted. Wait for it to finish ('pgrep -a disk-cleanup' shows it), then run again.")
    );
    let mut holder = holder;
    holder.child.kill().unwrap();
    holder.child.wait().unwrap();
    // flock dies with the holder: a crashed run never leaves a stale lock
    assert_eq!(guard.acquire(), Ok(()));
    let again = RealRunGuard::new(p, None, Arc::new(SignalState::new()));
    assert!(
        matches!(again.acquire(), Err(FatalError::LockHeld { pid }) if pid == std::process::id())
    );
}

#[test]
fn probe_should_report_the_holder_without_taking_the_lock_or_creating_the_file() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("run.lock");
    assert_eq!(RunLock::probe(&p), None);
    assert!(!p.exists());
    let holder = spawn_role("hold", p.to_str().unwrap(), None);
    holder.expect("ready");
    let _turn = fork_turn();
    assert_eq!(RunLock::probe(&p), Some(holder.child.id()));
    assert_eq!(RunLock::probe(&p), Some(holder.child.id()));
    let mut holder = holder;
    // the holder kept its lock through both probes: a real acquire still refuses
    assert!(matches!(
        RunLock::try_acquire(&p, None),
        Err(LockError::Held { .. })
    ));
    holder.child.kill().unwrap();
    holder.child.wait().unwrap();
    assert_eq!(RunLock::probe(&p), None);
}

#[test]
fn interrupt_should_finish_the_in_flight_item_and_stop_with_the_signal_When_sigint_or_sigterm_arrives(
) {
    for (sig, exit) in [(libc::SIGINT, 130), (libc::SIGTERM, 143)] {
        let c = spawn_role("items", "", None);
        c.expect("ready");
        c.expect("item 3");
        c.signal(sig);
        let (code, out, _) = c.finish();
        assert_eq!(code, exit, "{out:?}");
        assert!(
            out.contains(&format!("interrupted sig={sig} attempted=3")),
            "{out:?}"
        );
        assert!(!out.iter().any(|l| l.starts_with("item 4")), "{out:?}");
    }
}

#[test]
fn second_sigint_should_exit_at_once_When_no_command_child_runs() {
    for first in [libc::SIGINT, libc::SIGTERM] {
        let c = spawn_role("stuck", "", None);
        c.expect("ready");
        c.signal(first);
        std::thread::sleep(Duration::from_millis(300));
        let t = Instant::now();
        c.signal(libc::SIGINT);
        let (code, _, err) = c.finish();
        assert_eq!(code, 130, "{err:?}");
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(!err.iter().any(|l| l.contains("Ctrl-C again")), "{err:?}");
    }
}

#[test]
fn plan_mode_should_print_the_scan_line_and_exit_without_waiting_When_signalled() {
    for (sig, exit) in [(libc::SIGINT, 130), (libc::SIGTERM, 143)] {
        let c = spawn_role("plan", "", None);
        c.expect("ready");
        c.signal(sig);
        let t = Instant::now();
        let (code, out, err) = c.finish();
        assert_eq!(code, exit);
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(err.iter().any(|l| l == SCAN_INTERRUPTED), "{err:?}");
        assert!(!out.iter().any(|l| l.contains("scan finished")));
    }
}

fn fake_git(dir: &Path) {
    let script = dir.join("git");
    // reports its pid and process group, blocks for 2 s, then prints its own outcome
    std::fs::write(
        &script,
        "#!/bin/sh\ncd \"$(dirname \"$0\")\"\necho \"pid=$$ pgrp=$(cut -d' ' -f5 /proc/$$/stat)\" > started.tmp\nmv started.tmp started\nsleep 2\ntouch done\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn wait_for_git(dir: &Path) -> String {
    let started = dir.join("started");
    for _ in 0..400 {
        if let Ok(s) = std::fs::read_to_string(&started) {
            return s.trim().to_string();
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("fake git never started");
}

#[test]
fn worktree_remove_should_run_in_its_own_group_and_be_waited_for_When_two_sigints_arrive() {
    for (first, exit) in [(libc::SIGINT, 130), (libc::SIGTERM, 143)] {
        let d = tempfile::tempdir().unwrap();
        fake_git(d.path());
        let c = spawn_role("worktree", "", Some(d.path()));
        c.expect("ready");
        let git = wait_for_git(d.path());
        let t = Instant::now();
        c.signal(first);
        std::thread::sleep(Duration::from_millis(200));
        c.signal(libc::SIGINT);
        std::thread::sleep(Duration::from_millis(200));
        c.signal(libc::SIGINT);
        let child_pgrp = getpgid(Some(c.pid())).unwrap().as_raw_nonzero().get();
        let (code, out, err) = c.finish();
        let field = |k: &str| -> i32 {
            git.split(' ')
                .find_map(|w| w.strip_prefix(k))
                .unwrap()
                .parse()
                .unwrap()
        };
        assert_eq!(field("pgrp="), field("pid="), "git leads its own group");
        assert_ne!(field("pgrp="), child_pgrp);
        assert!(
            t.elapsed() >= Duration::from_millis(1400),
            "exit waited for git"
        );
        assert_eq!(code, exit, "{out:?} {err:?}");
        assert!(
            d.path().join("done").exists(),
            "git ran to its end, unsignalled"
        );
        assert!(out.contains(&"git returned ok=true".to_string()), "{out:?}");
        let acks: Vec<_> = err.iter().filter(|l| l.contains("Ctrl-C again")).collect();
        // the first signal stops the run; each later SIGINT is acknowledged
        assert_eq!(acks.len(), 2, "{err:?}");
        assert_eq!(
            acks[0].as_str(),
            format!("Ctrl-C again is ignored while 'git worktree remove {WT}' runs; the run stops when it ends.")
        );
    }
}

#[test]
fn one_sigint_during_a_worktree_remove_should_not_be_acknowledged_and_stops_after_git() {
    let d = tempfile::tempdir().unwrap();
    fake_git(d.path());
    let c = spawn_role("worktree", "", Some(d.path()));
    c.expect("ready");
    wait_for_git(d.path());
    c.signal(libc::SIGINT);
    let (code, out, err) = c.finish();
    assert_eq!(code, 130);
    assert!(out.contains(&"git returned ok=true".to_string()), "{out:?}");
    assert!(!err.iter().any(|l| l.contains("Ctrl-C again")), "{err:?}");
}

#[test]
fn closed_stdout_should_not_fail_writes_or_kill_the_process() {
    let mut c = spawn_role("pipe", "", None);
    c.expect("ready");
    let mut stdin = c.child.stdin.take().unwrap();
    // the stdout reader thread of spawn_role holds the read end; wait for it to see EOF-less close
    std::thread::sleep(Duration::from_millis(200));
    writeln!(stdin, "go").unwrap();
    let (code, _, err) = c.finish();
    assert_eq!(code, 0, "{err:?}");
    assert!(
        err.iter().any(|l| l == "DC:pipe failed=0 closed=true"),
        "{err:?}"
    );
}

#[test]
fn machine_should_follow_the_rules_of_the_story() {
    let mut m = Machine::new(Mode::Apply);
    assert_eq!(m.on_signal(2, None), Reaction::Abort);
    assert_eq!(m.on_signal(15, None), Reaction::Ignore);
    assert!(matches!(
        m.on_signal(2, Some("c")),
        Reaction::Acknowledge(_)
    ));
    assert_eq!(
        m.on_signal(2, None),
        Reaction::Exit {
            code: 130,
            line: None
        }
    );
    let mut p = Machine::new(Mode::Plan);
    assert_eq!(
        p.on_signal(15, None),
        Reaction::Exit {
            code: 143,
            line: Some(SCAN_INTERRUPTED.into())
        }
    );
}

#[test]
fn child_in_flight_should_be_cleared_when_dropped() {
    drop(ChildInFlight::begin("x".into()));
}

#[test]
fn swallow_should_pass_writes_through_when_the_pipe_is_open() {
    let mut buf = Vec::new();
    {
        let mut w = SwallowBrokenPipe::new(&mut buf);
        writeln!(w, "hello").unwrap();
        assert!(!w.is_closed());
    }
    assert_eq!(buf, b"hello\n");
}

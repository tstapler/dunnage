//! Test helper for Story 6.3.3 (built only with `test-support`): runs `RealTermSignals`, prints `ready`,
//! then reports on stdout what it sees (`interrupted N`, `tstp`, `resumed`) while a test sends it signals.
//! Commands on stdin: `suspend` calls `suspend()` and prints `resumed` when it returns; `quit` exits.
//! With the argument `apply` it installs the run's apply-mode handlers instead (SIGINT and SIGTERM only), to
//! show that SIGHUP keeps its default action there.

use std::io::{BufRead, Write};
use std::sync::mpsc::{channel, TryRecvError};
use std::time::Duration;

use disk_cleanup_adapters::RealTermSignals;
use disk_cleanup_core::explore::ports::TermSignals;

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn apply_mode() {
    let state = std::sync::Arc::new(disk_cleanup_adapters::signals::SignalState::new());
    let installed = disk_cleanup_adapters::signals::install(
        disk_cleanup_adapters::signals::Mode::Apply,
        state,
        Box::new(std::io::sink()),
    );
    if installed.is_err() {
        std::process::exit(2);
    }
    say("ready");
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        if line == "quit" {
            return;
        }
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("apply") {
        return apply_mode();
    }
    let Ok(signals) = RealTermSignals::new() else {
        std::process::exit(2);
    };
    let (tx, rx) = channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    say("ready");
    let mut reported = None;
    loop {
        let now = signals.interrupted();
        if now != reported {
            if let Some(n) = now {
                say(&format!("interrupted {n}"));
            }
            reported = now;
        }
        if signals.suspend_requested() {
            say("tstp");
        }
        match rx.try_recv() {
            Ok(cmd) if cmd == "suspend" => {
                signals.suspend();
                say("resumed");
            }
            Ok(cmd) if cmd == "quit" => return,
            Ok(_) | Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return,
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

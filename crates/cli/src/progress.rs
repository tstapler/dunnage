//! Progress and the stall watchdog on stderr (Story 1.2.4, Task 1.2.4b, UX-59, `design/ux.md` S9).
//!
//! On one shared terminal a per-item line would interleave with the rows on stdout, so the lines that name
//! items run only when stdout is redirected and stderr is a terminal. Everything is plain lines, at most one
//! per two seconds, never a carriage return.

use std::io::Write;

/// Which progress output is on. The watchdog warning is not here: it is always on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressMode {
    /// `[n/m] removing ...` lines of an apply.
    pub per_item: bool,
    /// `scanning ...` lines of a scan.
    pub scanning: bool,
}

// The five inputs are the truth table of UX-59.
#[allow(clippy::fn_params_excessive_bools)]
pub fn progress_mode(
    stdout_tty: bool,
    stderr_tty: bool,
    quiet: bool,
    json: bool,
    no_progress: bool,
) -> ProgressMode {
    let silenced = quiet || json || no_progress;
    ProgressMode {
        per_item: !stdout_tty && stderr_tty && !silenced,
        scanning: stderr_tty && !silenced,
    }
}

/// Lines closer together than this are dropped (apart from the first).
pub const MIN_LINE_GAP_MS: u64 = 2_000;
/// No progress for this long prints the watchdog hint once per stall.
pub const STALL_AFTER_MS: u64 = 15_000;

/// Writes progress lines to `sink` with the time taken from the caller, so a test needs no clock.
pub struct Progress<'a> {
    mode: ProgressMode,
    sink: &'a mut dyn Write,
    last_line_ms: Option<u64>,
    last_progress_ms: u64,
    warned: bool,
}

impl<'a> Progress<'a> {
    pub fn new(mode: ProgressMode, sink: &'a mut dyn Write, now_ms: u64) -> Progress<'a> {
        Progress {
            mode,
            sink,
            last_line_ms: None,
            last_progress_ms: now_ms,
            warned: false,
        }
    }

    fn line(&mut self, now_ms: u64, text: &str) {
        self.last_progress_ms = now_ms;
        self.warned = false;
        if self
            .last_line_ms
            .is_some_and(|t| now_ms.saturating_sub(t) < MIN_LINE_GAP_MS)
        {
            return;
        }
        self.last_line_ms = Some(now_ms);
        // A closed stderr is not worth stopping a run for.
        let _ = writeln!(self.sink, "{text}");
    }

    /// `[n/m] removing <what>`.
    pub fn removing(&mut self, now_ms: u64, n: usize, m: usize, what: &str) {
        if self.mode.per_item {
            self.line(now_ms, &format!("[{n}/{m}] removing {what}"));
        } else {
            self.last_progress_ms = now_ms;
            self.warned = false;
        }
    }

    pub fn scanning(&mut self, now_ms: u64, what: &str) {
        if self.mode.scanning {
            self.line(now_ms, &format!("scanning {what}"));
        } else {
            self.last_progress_ms = now_ms;
            self.warned = false;
        }
    }

    /// The watchdog: after 15 s with no progress, one stderr warning (until progress resumes).
    pub fn watchdog(&mut self, now_ms: u64) -> bool {
        let stalled = now_ms.saturating_sub(self.last_progress_ms) >= STALL_AFTER_MS;
        if !stalled || self.warned {
            return false;
        }
        self.warned = true;
        let secs = now_ms.saturating_sub(self.last_progress_ms) / 1000;
        let _ = writeln!(
            self.sink,
            "warning: no progress for {secs}s; a process may be blocked in an uninterruptible filesystem call (see 'cat /proc/<pid>/stack' for the disk-cleanup pid). Waiting."
        );
        true
    }
}

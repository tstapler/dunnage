//! The pure rules of the `btrfs` balance ladder (Story 4.3.2, Task 4.3.2a): which `-dusage` step comes next,
//! when the ladder stops and why, which running jobs forbid a start, and the argv of a step. Nothing here
//! runs a command or reads the machine; `adapters::targets::btrfs_balance` feeds it facts and obeys it.

use std::ffi::OsString;
use std::path::Path;

use crate::outcome::SkipReason;
use crate::ports::Cmd;
use crate::preflight::MIN_UNALLOCATED_BYTES;
use crate::target::Guarded;

/// The `-dusage` values in escalation order: 0 moves only empty chunks, the others move chunks at most that
/// full (`plan.md` Story 4.3.2).
pub const LADDER: [u8; 5] = [0, 5, 10, 20, 40];

/// The most chunks one non-zero step may relocate; part of the `-d` filter (`-dusage=N,limit=8`).
pub const CHUNK_LIMIT: u32 = 8;

/// Below this much unallocated space a non-zero step has no room to write the relocated chunk into
/// (`pitfalls.md` #6; the owner confirms the threshold before the live run, plan "Unresolved Questions").
pub const MIN_UNALLOCATED_FOR_MOVE: u64 = MIN_UNALLOCATED_BYTES;

/// The ladder stops once this much is unallocated: the `status` WARN threshold (`scan::health`,
/// `warn_unallocated`), below which the filesystem is not healthy.
pub const TARGET_UNALLOCATED: u64 = 10 << 30;

/// What the summary and stderr say when a balance is left running by an interrupt (plan Story 4.3.2, E-8).
pub const BALANCE_STILL_RUNNING: &str = "INTERRUPTED: a btrfs balance is still running in the kernel. This tool does not stop it. Check it with 'sudo btrfs balance status /'; stop it yourself with 'sudo btrfs balance cancel /'.";

/// The argv of the step `-dusage=<usage>` on `mount`. `-dusage=0` carries no limit (it moves only chunks with
/// no data); every other step is bounded by [`CHUNK_LIMIT`]. A `usage` outside [`LADDER`] has no allowlist
/// row, so the runner refuses it.
#[must_use]
pub fn balance_cmd(usage: u8, mount: &Path) -> Cmd {
    let filter = if usage == 0 {
        "-dusage=0".to_string()
    } else {
        format!("-dusage={usage},limit={CHUNK_LIMIT}")
    };
    Cmd::new(
        "btrfs",
        [
            OsString::from("balance"),
            OsString::from("start"),
            OsString::from(filter),
            mount.as_os_str().to_owned(),
        ],
        true,
    )
}

/// What forbids starting a balance now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocker {
    /// `btrfs balance status` reports a running or paused balance.
    BalanceRunning,
    /// `btrfs balance status` failed or printed something unknown: fail closed.
    BalanceStatusUnknown,
    ScrubRunning,
    /// `btrfs-balance.timer` is active, so a balance may start on its own.
    TimerActive,
}

impl Blocker {
    /// The note that names which job is in the way.
    #[must_use]
    pub fn note(self) -> &'static str {
        match self {
            Blocker::BalanceRunning => {
                "a btrfs balance is already running or paused (check 'sudo btrfs balance status /')"
            }
            Blocker::BalanceStatusUnknown => {
                "'btrfs balance status' did not give a recognised answer, so a running balance cannot be ruled out"
            }
            Blocker::ScrubRunning => "a btrfs scrub is running",
            Blocker::TimerActive => "btrfs-balance.timer is active",
        }
    }
}

/// `btrfs balance status <mnt>`: `No balance found on '/'` when idle, `Balance on '/' is running` (or
/// `paused`, `running, cancel requested`) otherwise. `None` means no blocker.
#[must_use]
pub fn balance_blocker(status_stdout: &str) -> Option<Blocker> {
    if status_stdout.contains("No balance found") {
        None
    } else if status_stdout.contains("is running") || status_stdout.contains("is paused") {
        Some(Blocker::BalanceRunning)
    } else {
        Some(Blocker::BalanceStatusUnknown)
    }
}

/// `btrfs scrub status <mnt>` prints a `Status:` line (`running`, `finished`, `aborted`, ...).
#[must_use]
pub fn scrub_blocker(status_stdout: &str) -> Option<Blocker> {
    status_stdout
        .lines()
        .filter_map(|l| l.trim().strip_prefix("Status:"))
        .any(|v| v.trim().starts_with("running"))
        .then_some(Blocker::ScrubRunning)
}

/// `systemctl is-active btrfs-balance.timer` prints `active` and exits 0 only for an active unit.
#[must_use]
pub fn timer_blocker(is_active_stdout: &str) -> Option<Blocker> {
    (is_active_stdout.trim() == "active").then_some(Blocker::TimerActive)
}

/// The note of a `Skip(AlreadyRunning)`: every blocker found, in order.
#[must_use]
pub fn already_running_note(blockers: &[Blocker]) -> String {
    let named: Vec<&str> = blockers.iter().map(|b| b.note()).collect();
    format!("not started: {}", named.join("; "))
}

/// Whether the ladder may start: a non-root user is told `NeedsRoot` first (the tool never calls `sudo`),
/// then any running job is `AlreadyRunning`.
#[must_use]
pub fn decide_start(euid: u32, blockers: &[Blocker]) -> Guarded {
    if euid != 0 {
        Guarded::Skip(SkipReason::NeedsRoot)
    } else if blockers.is_empty() {
        Guarded::Allow
    } else {
        Guarded::Skip(SkipReason::AlreadyRunning)
    }
}

/// What `btrfs balance start` printed: `Done, had to relocate 3 out of 120 chunks`. `None` when the line is
/// not there, which the ladder treats as unrecognised output rather than guessing a count.
#[must_use]
pub fn parse_relocated(output: &str) -> Option<u32> {
    output.lines().find_map(|l| {
        let rest = l.trim().strip_prefix("Done, had to relocate ")?;
        let (n, tail) = rest.split_once(' ')?;
        tail.starts_with("out of ").then_some(())?;
        n.parse().ok()
    })
}

/// One finished step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepResult {
    pub usage: u8,
    /// `None`: the output did not say.
    pub relocated: Option<u32>,
}

/// Why the ladder stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Every step of [`LADDER`] ran.
    Finished,
    /// Unallocated reached [`TARGET_UNALLOCATED`] (also when it already had before the first step).
    TargetReached,
    /// A non-zero step relocated no chunk.
    NothingToRelocate(u8),
    /// A non-zero step was due but unallocated is under [`MIN_UNALLOCATED_FOR_MOVE`] (or unreadable).
    TooLittleUnallocated { next: u8, unallocated: Option<u64> },
    /// The last step's output carried no relocated count.
    UnrecognisedOutput(u8),
    /// A signal arrived between two steps.
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    Run(u8),
    Stop(StopReason),
}

/// The next step given the steps already run and the unallocated bytes just re-read (`None` if unreadable).
///
/// Decisions: a step that relocated 0 chunks ends the ladder, except `-dusage=0`, where 0 only means no
/// empty chunk existed and says nothing about chunks that are 5% full; and a step needing room to move a
/// chunk never runs below [`MIN_UNALLOCATED_FOR_MOVE`], so a starved filesystem gets `-dusage=0` alone.
#[must_use]
pub fn next_step(done: &[StepResult], unallocated: Option<u64>) -> Next {
    if unallocated.is_some_and(|u| u >= TARGET_UNALLOCATED) {
        return Next::Stop(StopReason::TargetReached);
    }
    if let Some(last) = done.last() {
        match last.relocated {
            None => return Next::Stop(StopReason::UnrecognisedOutput(last.usage)),
            Some(0) if last.usage != 0 => {
                return Next::Stop(StopReason::NothingToRelocate(last.usage))
            }
            Some(_) => {}
        }
    }
    let next = match done.last() {
        None => LADDER[0],
        Some(last) => match LADDER.iter().copied().find(|u| *u > last.usage) {
            Some(u) => u,
            None => return Next::Stop(StopReason::Finished),
        },
    };
    if next != 0 && !unallocated.is_some_and(|u| u >= MIN_UNALLOCATED_FOR_MOVE) {
        return Next::Stop(StopReason::TooLittleUnallocated { next, unallocated });
    }
    Next::Run(next)
}

fn size(n: u64) -> String {
    let (unit, div) = match n {
        n if n >= 1 << 30 => ("GiB", 1u64 << 30),
        n if n >= 1 << 20 => ("MiB", 1 << 20),
        _ => ("KiB", 1 << 10),
    };
    format!("{:.1} {unit}", n as f64 / div as f64)
}

/// The figure for a report line; an unreadable value says so instead of printing 0.
#[must_use]
pub fn unallocated_text(n: Option<u64>) -> String {
    n.map_or_else(|| "unknown".to_string(), size)
}

impl StopReason {
    /// The report line that says why the ladder ended; for the precondition it says why a step did not run.
    #[must_use]
    pub fn note(self) -> String {
        match self {
            StopReason::Finished => {
                "balance ladder finished: every step -dusage=0,5,10,20,40 ran".into()
            }
            StopReason::TargetReached => format!(
                "balance stopped: unallocated reached the {} target",
                size(TARGET_UNALLOCATED)
            ),
            StopReason::NothingToRelocate(u) => format!(
                "balance stopped: -dusage={u} relocated 0 chunks, so no larger step was tried"
            ),
            StopReason::TooLittleUnallocated { next, unallocated } => format!(
                "balance step -dusage={next} and larger did not run: unallocated is {}, below the {} a chunk needs to be moved into (only -dusage=0 runs under that). Free some space first, then run it again.",
                unallocated_text(unallocated),
                size(MIN_UNALLOCATED_FOR_MOVE)
            ),
            StopReason::UnrecognisedOutput(u) => format!(
                "balance stopped after -dusage={u}: its output did not say how many chunks moved, so no larger step was tried"
            ),
            StopReason::Interrupted => {
                "balance stopped: interrupted between steps; no further step was started".into()
            }
        }
    }
}

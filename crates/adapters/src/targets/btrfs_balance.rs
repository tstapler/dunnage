//! The `btrfs` kind: a usage-filtered balance ladder behind `sudo ... --kinds btrfs --apply` (Story 4.3.2,
//! Task 4.3.2b). One candidate, the btrfs filesystem under HOME; its `execute` runs `-dusage=0,5,10,20,40`
//! through the `CommandRunner`, re-reading `unallocated` after every step (the rules are
//! `core::targets::btrfs_guard`).
//!
//! The tool never cancels a balance: it runs in the kernel and the `btrfs` command only waits for it. On a
//! signal the wait ends, the child is left alone in its own process group, and the item is `Status::Running`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use disk_cleanup_core::ports::{Cmd, Env};
use disk_cleanup_core::selector::Selection;
use disk_cleanup_core::target::{Guarded, KindGate, Target};
use disk_cleanup_core::targets::btrfs_guard::{
    already_running_note, balance_blocker, balance_cmd, decide_start, next_step, parse_relocated,
    scrub_blocker, timer_blocker, unallocated_text, Blocker, Next, StepResult, StopReason,
    BALANCE_STILL_RUNNING,
};
use disk_cleanup_core::ApplyToken;
use disk_cleanup_core::{
    AllocatedBytes, BtrfsFacts, Candidate, CandidateId, DeleteError, DeleteFailure, Freed,
    Identity, KindFacts, Locator, MountEntry, Outcome, Platform, SelectorId, SkipReason,
};

/// What one run of the ladder did, for the report lines.
#[derive(Debug, Default)]
struct RunLog {
    start_unallocated: Option<u64>,
    steps: Vec<(StepResult, Option<u64>)>,
    stop: Option<StopReason>,
    /// The ladder was aborted by ENOSPC: the step that failed.
    enospc_at: Option<u8>,
    /// A step failed for another reason: `(usage, text)`.
    failed: Option<(u8, String)>,
    /// The wait for this step was ended by a signal; the balance still runs.
    left_running: Option<u8>,
}

impl RunLog {
    /// `execute` never ran (every item was skipped), so there is nothing to report.
    fn is_untouched(&self) -> bool {
        self.steps.is_empty()
            && self.stop.is_none()
            && self.left_running.is_none()
            && self.enospc_at.is_none()
            && self.failed.is_none()
    }
}

pub struct BtrfsBalanceTarget {
    log: Mutex<RunLog>,
    /// Why a skipped item is `AlreadyRunning`, for the detail slot of its text.
    detail: Mutex<HashMap<CandidateId, String>>,
}

impl Default for BtrfsBalanceTarget {
    fn default() -> Self {
        Self::new()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The mount the balance runs on: the shortest mount point that shares a device field with the mount that
/// holds `home`, so every subvolume of one filesystem resolves to the same (usually `/`) mount. `home`
/// itself when the table is unreadable or has no entry above it (a balance accepts any path of the
/// filesystem).
#[must_use]
pub fn balance_mount(mounts: &[MountEntry], home: &Path) -> PathBuf {
    let Some(holder) = mounts
        .iter()
        .filter(|m| home.starts_with(&m.mount_point))
        .max_by_key(|m| m.mount_point.components().count())
    else {
        return home.to_path_buf();
    };
    mounts
        .iter()
        .filter(|m| m.dev == holder.dev)
        .min_by_key(|m| m.mount_point.components().count())
        .map_or_else(|| home.to_path_buf(), |m| m.mount_point.clone())
}

/// `btrfs <args> <mount>` as a read-only command.
fn btrfs_ro(args: [&str; 2], mount: &Path) -> Cmd {
    Cmd::new(
        "btrfs",
        [args[0].into(), args[1].into(), mount.as_os_str().to_owned()],
        false,
    )
}

impl BtrfsBalanceTarget {
    #[must_use]
    pub fn new() -> BtrfsBalanceTarget {
        BtrfsBalanceTarget {
            log: Mutex::new(RunLog::default()),
            detail: Mutex::new(HashMap::new()),
        }
    }

    fn unallocated(env: &Env, mount: &Path) -> Option<u64> {
        env.space.snapshot(mount).btrfs.map(|b| b.unallocated)
    }

    /// Every job that forbids a start, found with read-only commands. A balance status that cannot be read is
    /// a blocker (fail closed); a scrub status or timer query that fails is not, because both fail on a
    /// healthy host (no systemd, never scrubbed).
    fn blockers(env: &Env, mount: &Path) -> Vec<Blocker> {
        let mut out = Vec::new();
        match env.runner.run(&btrfs_ro(["balance", "status"], mount)) {
            Ok(o) => out.extend(balance_blocker(&format!(
                "{}{}",
                o.stdout_lossy(),
                String::from_utf8_lossy(&o.stderr)
            ))),
            Err(_) => out.push(Blocker::BalanceStatusUnknown),
        }
        if let Ok(o) = env.runner.run(&btrfs_ro(["scrub", "status"], mount)) {
            out.extend(scrub_blocker(&o.stdout_lossy()));
        }
        let timer = Cmd::new("systemctl", ["is-active", "btrfs-balance.timer"], false);
        if let Ok(o) = env.runner.run(&timer) {
            out.extend(timer_blocker(&o.stdout_lossy()));
        }
        out
    }

    fn mount_of(c: &Candidate) -> Option<&Path> {
        match c.locator() {
            Locator::Path(p) => Some(p),
            _ => None,
        }
    }

    fn failure(error: DeleteError) -> DeleteFailure {
        DeleteFailure {
            error,
            freed: Freed::default(),
        }
    }
}

fn is_enospc(e: &DeleteError) -> bool {
    match e {
        DeleteError::NoSpace | DeleteError::Io(28) => true,
        DeleteError::CommandFailed { message, .. } => message.contains("No space left on device"),
        _ => false,
    }
}

/// `+38.0 GiB, ` from the two re-read values; an empty string when either is unreadable, and `did not grow, `
/// when the balance returned no more than 1 GiB, so the line never claims space it cannot show (UX-12).
fn unallocated_change(before: Option<u64>, after: Option<u64>) -> String {
    let (Some(b), Some(a)) = (before, after) else {
        return String::new();
    };
    if a <= b.saturating_add(1 << 30) {
        return format!("unallocated did not grow, {}, ", signed_size(a, b));
    }
    format!("{}, ", signed_size(a, b))
}

fn signed_size(after: u64, before: u64) -> String {
    let (sign, d) = if after >= before {
        ('+', after - before)
    } else {
        ('-', before - after)
    };
    if d == 0 {
        return "+0 B".to_string();
    }
    format!("{sign}{}", unallocated_text(Some(d)))
}

impl Target for BtrfsBalanceTarget {
    fn name(&self) -> &'static str {
        "btrfs"
    }

    fn selectors(&self) -> &'static [SelectorId] {
        &[SelectorId::Btrfs]
    }

    fn gate(&self, env: &Env) -> KindGate {
        if env.probe.platform() != Platform::Linux {
            return KindGate::Unavailable(SkipReason::UnsupportedOnThisOS);
        }
        if !env.runner.exists("btrfs") {
            return KindGate::Unavailable(SkipReason::ToolMissing);
        }
        KindGate::Available
    }

    /// The `kinds` table says "needs root" for a user; the run itself reports the item `Skip(NeedsRoot)`.
    fn selector_gate(&self, _selector: SelectorId, env: &Env) -> KindGate {
        if env.probe.euid() != 0 {
            return KindGate::Unavailable(SkipReason::NeedsRoot);
        }
        self.gate(env)
    }

    fn unavailable_detail(&self, env: &Env) -> Option<String> {
        (env.probe.platform() == Platform::Linux).then(|| "btrfs".to_string())
    }

    fn discover(&self, env: &Env, _selection: &Selection) -> Vec<Candidate> {
        let on_btrfs = env.space.snapshot(&env.dirs.home).fs_type == "btrfs";
        if !on_btrfs {
            return Vec::new();
        }
        let mounts = env.probe.mounts().unwrap_or_default();
        let mount = balance_mount(&mounts, &env.dirs.home);
        vec![Candidate::new(
            Locator::Path(mount),
            KindFacts::Btrfs(BtrfsFacts::Balance { usage_pct: 40 }),
            AllocatedBytes(0),
            None,
            Identity { dev: 0, ino: 0 },
        )]
    }

    fn guard(&self, c: &Candidate, env: &Env) -> Guarded {
        let Some(mount) = Self::mount_of(c) else {
            return Guarded::Skip(SkipReason::OutsideRoot);
        };
        // Root first and alone: a non-root plan runs no command at all.
        if env.probe.euid() != 0 {
            return decide_start(env.probe.euid(), &[]);
        }
        let blockers = Self::blockers(env, mount);
        let verdict = decide_start(0, &blockers);
        if verdict != Guarded::Allow {
            lock(&self.detail).insert(c.id(), already_running_note(&blockers));
        }
        verdict
    }

    fn detail(&self, c: &Candidate) -> Option<String> {
        lock(&self.detail).get(&c.id()).cloned()
    }

    fn execute(
        &self,
        c: &Candidate,
        env: &Env,
        proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        let Some(mount) = Self::mount_of(c) else {
            return Err(Self::failure(DeleteError::OutsideScope));
        };
        *lock(&self.log) = RunLog::default();
        let mut unallocated = Self::unallocated(env, mount);
        lock(&self.log).start_unallocated = unallocated;
        let mut done: Vec<StepResult> = Vec::new();
        loop {
            let usage = match next_step(&done, unallocated) {
                Next::Stop(reason) => {
                    lock(&self.log).stop = Some(reason);
                    return Ok(Freed::default());
                }
                Next::Run(u) => u,
            };
            if env.guard.abort_flag().is_set() {
                lock(&self.log).stop = Some(StopReason::Interrupted);
                return Ok(Freed::default());
            }
            let cmd = balance_cmd(usage, mount);
            match env
                .runner
                .run_mutating_unless_aborted(&cmd, proof, env.guard.abort_flag())
            {
                Ok(None) => {
                    lock(&self.log).left_running = Some(usage);
                    return Ok(Freed::default());
                }
                Ok(Some(out)) => {
                    let text = format!(
                        "{}{}",
                        out.stdout_lossy(),
                        String::from_utf8_lossy(&out.stderr)
                    );
                    let step = StepResult {
                        usage,
                        relocated: parse_relocated(&text),
                    };
                    unallocated = Self::unallocated(env, mount);
                    lock(&self.log).steps.push((step, unallocated));
                    done.push(step);
                }
                Err(e) if is_enospc(&e) => {
                    lock(&self.log).enospc_at = Some(usage);
                    return Err(Self::failure(DeleteError::NoSpace));
                }
                Err(e) => {
                    lock(&self.log).failed = Some((usage, e.text()));
                    return Err(Self::failure(e));
                }
            }
        }
    }

    fn left_running(&self, c: &Candidate) -> bool {
        let _ = c;
        lock(&self.log).left_running.is_some()
    }

    fn finish(&self, outcomes: &[Outcome], env: &Env) -> Vec<String> {
        let log = lock(&self.log);
        if outcomes.is_empty() || log.is_untouched() {
            return Vec::new();
        }
        let mut notes = Vec::new();
        let mount = outcomes
            .first()
            .and_then(|o| o.path.as_deref())
            .unwrap_or(&env.dirs.home);
        let was = unallocated_text(log.start_unallocated);
        for (s, after) in &log.steps {
            let moved = s
                .relocated
                .map_or_else(|| "an unknown number of".into(), |n| n.to_string());
            notes.push(format!(
                "btrfs balance -dusage={} relocated {moved} chunks; unallocated now {}",
                s.usage,
                unallocated_text(*after)
            ));
        }
        if let Some(u) = log.left_running {
            notes.push(format!(
                "btrfs balance -dusage={u} was still running when the wait ended"
            ));
            notes.push(BALANCE_STILL_RUNNING.to_string());
        }
        if let Some(u) = log.enospc_at {
            let now = log.steps.last().map_or(log.start_unallocated, |(_, a)| *a);
            notes.push(format!(
                "btrfs balance -dusage={u} failed with ENOSPC; the ladder was aborted. Unallocated was {was}, now {}. See 'sudo btrfs filesystem usage {}'.",
                unallocated_text(now),
                mount.display()
            ));
        }
        if let Some((u, text)) = &log.failed {
            notes.push(format!(
                "btrfs balance -dusage={u} failed: {text}; the ladder was aborted"
            ));
        }
        if let Some(stop) = log.stop {
            notes.push(stop.note());
        }
        if !log.steps.is_empty() {
            let last = log.steps.last().and_then(|(_, a)| *a);
            notes.push(format!(
                "btrfs unallocated: {was} before, {} after ({}re-read from the filesystem)",
                unallocated_text(last),
                unallocated_change(log.start_unallocated, last)
            ));
        }
        notes
    }
}

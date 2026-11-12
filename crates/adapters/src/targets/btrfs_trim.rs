//! The `btrfs-trim` kind: `fstrim -v <mount>` behind `sudo ... --kinds btrfs-trim --apply` (Story 4.3.3,
//! Task 4.3.3a). One candidate, the mount of the btrfs filesystem under HOME. A trim hands blocks back to the
//! device; it frees no filesystem space, so the figure it prints is recorded as `trimmed_bytes` and never as
//! freed (UX-14).
//!
//! There is no SSD check: `fstrim` itself refuses a device without discard support ("the discard operation is
//! not supported"), which ends the item as an `Error` with that text and changes nothing.

use std::collections::HashMap;
use std::sync::Mutex;

use dunnage_core::ports::Env;
use dunnage_core::selector::Selection;
use dunnage_core::target::{Guarded, KindGate, Target};
use dunnage_core::targets::btrfs_guard::{decide_trim, parse_trimmed, trim_cmd};
use dunnage_core::ApplyToken;
use dunnage_core::{
    AllocatedBytes, BtrfsFacts, Candidate, CandidateId, DeleteError, DeleteFailure, Freed,
    Identity, KindFacts, Locator, Outcome, Platform, SelectorId, SkipReason,
};

use super::btrfs_balance::balance_mount;

#[derive(Debug, Clone, Copy)]
enum Trimmed {
    Bytes(u64),
    /// `fstrim` succeeded but its output carried no figure.
    Unreported,
}

#[derive(Default)]
pub struct BtrfsTrimTarget {
    trimmed: Mutex<HashMap<CandidateId, Trimmed>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl BtrfsTrimTarget {
    #[must_use]
    pub fn new() -> BtrfsTrimTarget {
        BtrfsTrimTarget::default()
    }
}

impl Target for BtrfsTrimTarget {
    fn name(&self) -> &'static str {
        "btrfs-trim"
    }

    fn selectors(&self) -> &'static [SelectorId] {
        &[SelectorId::BtrfsTrim]
    }

    fn gate(&self, env: &Env) -> KindGate {
        if env.probe.platform() != Platform::Linux {
            return KindGate::Unavailable(SkipReason::UnsupportedOnThisOS);
        }
        if !env.runner.exists("fstrim") {
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
        (env.probe.platform() == Platform::Linux).then(|| "fstrim".to_string())
    }

    fn discover(&self, env: &Env, _selection: &Selection) -> Vec<Candidate> {
        if env.space.snapshot(&env.dirs.home).fs_type != "btrfs" {
            return Vec::new();
        }
        let mounts = env.probe.mounts().unwrap_or_default();
        vec![Candidate::new(
            Locator::Path(balance_mount(&mounts, &env.dirs.home)),
            KindFacts::Btrfs(BtrfsFacts::Trim),
            AllocatedBytes(0),
            None,
            Identity { dev: 0, ino: 0 },
        )]
    }

    fn guard(&self, c: &Candidate, env: &Env) -> Guarded {
        if !matches!(c.locator(), Locator::Path(_)) {
            return Guarded::Skip(SkipReason::OutsideRoot);
        }
        decide_trim(env.probe.euid())
    }

    fn execute(
        &self,
        c: &Candidate,
        env: &Env,
        proof: &ApplyToken,
    ) -> Result<Freed, DeleteFailure> {
        let fail = |error| DeleteFailure {
            error,
            freed: Freed::default(),
        };
        let Locator::Path(mount) = c.locator() else {
            return Err(fail(DeleteError::OutsideScope));
        };
        let out = env
            .runner
            .run_mutating(&trim_cmd(mount), proof)
            .map_err(fail)?;
        let text = format!(
            "{}{}",
            out.stdout_lossy(),
            String::from_utf8_lossy(&out.stderr)
        );
        let figure = parse_trimmed(&text).map_or(Trimmed::Unreported, Trimmed::Bytes);
        lock(&self.trimmed).insert(c.id(), figure);
        Ok(Freed::default())
    }

    fn trimmed_bytes(&self, c: &Candidate) -> Option<u64> {
        match lock(&self.trimmed).get(&c.id()) {
            Some(Trimmed::Bytes(n)) => Some(*n),
            _ => None,
        }
    }

    fn finish(&self, outcomes: &[Outcome], _env: &Env) -> Vec<String> {
        let trimmed = lock(&self.trimmed);
        outcomes
            .iter()
            .filter_map(|o| match trimmed.get(&o.id)? {
                Trimmed::Bytes(_) => Some("fstrim trimmed blocks on the device; that is not freed filesystem space and does not change 'unallocated'".to_string()),
                Trimmed::Unreported => Some("fstrim succeeded but did not report how much it trimmed".to_string()),
            })
            .take(1)
            .collect()
    }
}

//! The `Target` trait (Story 1.1.4, Task 1.1.4a): one cleanable family's discovery, guard and removal. The
//! executor owns ordering, the worker pool, the audit trail and the books; a target only implements behaviour.

use crate::apply_token::ApplyToken;
use crate::candidate::Candidate;
use crate::delete_types::{DeleteFailure, Freed};
use crate::kind::{Kind, SelectorId};
use crate::outcome::{Outcome, SkipReason};
use crate::platform::Platform;
use crate::ports::Env;
use crate::selector::Selection;

/// What the report and the `kinds` AVAILABLE column say when a kind has not shipped in this build.
pub const NOT_IN_BUILD: &str = "not available in this build";

/// Whole-kind availability, asked once per run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindGate {
    Available,
    /// Items are shown, none is removable (`DockerVersionUntested`).
    ListOnly(SkipReason),
    /// The whole kind is skipped (`DaemonUnavailable`, `NonLocalDockerContext`, `UnsupportedOnThisOS`,
    /// `UnrecognisedOutput`, `LibraryUnmounted`, `SteamRunning`, `NeedsRoot`, `AlreadyRunning`, `ToolMissing`).
    Unavailable(SkipReason),
}

impl KindGate {
    pub fn is_unavailable(&self) -> bool {
        matches!(self, KindGate::Unavailable(_))
    }

    /// The AVAILABLE column of `kinds`, derived from the gate and never computed elsewhere.
    pub fn available_label(&self, platform: Platform) -> String {
        match self {
            KindGate::Available => "yes".into(),
            KindGate::ListOnly(_) => "list only".into(),
            KindGate::Unavailable(SkipReason::DaemonUnavailable) => "daemon down".into(),
            KindGate::Unavailable(SkipReason::NeedsRoot) => "needs root".into(),
            KindGate::Unavailable(SkipReason::UnsupportedOnThisOS) => match platform {
                Platform::Macos => "not supported on macOS".into(),
                Platform::Linux => "not supported on Linux".into(),
            },
            KindGate::Unavailable(r) => r.reason_text().into(),
        }
    }

    /// The once-per-kind line for an unavailable kind. In plan mode `docker: SKIPPED (daemon not reachable:
    /// <detail>)`, in apply mode `docker: could not run (daemon not reachable: <detail>)`.
    pub fn skipped_line(
        &self,
        family: &str,
        detail: Option<&str>,
        applying: bool,
    ) -> Option<String> {
        let KindGate::Unavailable(reason) = self else {
            return None;
        };
        let what = if *reason == SkipReason::DaemonUnavailable {
            format!(
                "daemon not reachable{}",
                detail.map_or(String::new(), |d| format!(": {d}"))
            )
        } else {
            // The slots a whole-kind skip can fill from the detail (the unmounted library, the tool).
            let text = reason.reason_text();
            match (reason, detail) {
                (SkipReason::LibraryUnmounted, Some(d)) => text.replace("<path>", d),
                (SkipReason::UnrecognisedOutput, Some(d)) => text.replace("<tool>", d),
                _ => text.to_string(),
            }
        };
        let verb = if applying { "could not run" } else { "SKIPPED" };
        Some(format!("{family}: {verb} ({what})"))
    }
}

/// A guard decision. Re-asked immediately before `execute`: an item that was `Allow` at scan time and is
/// `Skip` now is skipped, never executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guarded {
    Allow,
    Skip(SkipReason),
}

/// Per-item facts a target's probe produced, for the placeholders of the reason texts and the plan row
/// (Story 3.1.3). Every field is optional; an absent fact drops its clause.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemFacts {
    /// Unpushed commits (`UnpushedCommits`).
    pub unpushed: Option<u32>,
    /// First holder of the item, process name and pid (`InUse`).
    pub process: Option<(String, u32)>,
    /// Count and `treestat` size of the ignored files git would delete (`HasIgnoredFiles`).
    pub ignored: Option<(u64, u64)>,
    /// Count and size of the ignored files an allowed item will discard (`--worktree-ignored discard`).
    pub discards: Option<(u64, u64)>,
}

/// One family of cleanable things.
///
/// `execute` cannot be called without a token value:
///
/// ```compile_fail
/// use disk_cleanup_core::{target::Target, Candidate, ports::Env};
/// fn no_token(t: &dyn Target, c: &Candidate, env: &Env) {
///     let _ = t.execute(c, env);
/// }
/// ```
pub trait Target: Send + Sync {
    /// The family name used in `SKIPPED` lines (`docker`, `steam`).
    fn name(&self) -> &'static str;

    /// The selectors this target serves; it is asked for candidates only when one of them is selected.
    fn selectors(&self) -> &'static [SelectorId];

    fn gate(&self, _env: &Env) -> KindGate {
        KindGate::Available
    }

    /// The availability of one selector, for the AVAILABLE column of `kinds` only (a family can have a
    /// selector that needs root while its sibling does not). The run itself asks [`Target::gate`].
    fn selector_gate(&self, _selector: SelectorId, env: &Env) -> KindGate {
        self.gate(env)
    }

    /// The `<reason>` of the `SKIPPED` line, for example why the Docker daemon is not reachable.
    fn unavailable_detail(&self, _env: &Env) -> Option<String> {
        None
    }

    fn discover(&self, env: &Env, selection: &Selection) -> Vec<Candidate>;

    /// Impure probe plus pure decision, asked at plan time and again right before `execute`.
    fn guard(&self, c: &Candidate, env: &Env) -> Guarded;

    /// Remove one candidate. The result carries the partial `Freed` of a failed removal.
    fn execute(&self, c: &Candidate, env: &Env, proof: &ApplyToken)
        -> Result<Freed, DeleteFailure>;

    /// The detail slot of an item's skip text, for example the tool a `ToolMissing` row names (`pip3`).
    fn detail(&self, _c: &Candidate) -> Option<String> {
        None
    }

    /// The probe facts behind an item's skip text and plan row; empty unless the target keeps them.
    fn item_facts(&self, _c: &Candidate) -> ItemFacts {
        ItemFacts::default()
    }

    /// The guard asked right before `execute` (`Executor::run_one`). It defaults to [`Target::guard`]; a target
    /// whose plan-time guard is deliberately cheap overrides it with the full, fresh re-check (a tree walk, a
    /// process scan) so that cost is paid once per item that is really about to be removed.
    fn guard_before_delete(&self, c: &Candidate, env: &Env) -> Guarded {
        self.guard(c, env)
    }

    /// Lines for the `Notes:` block of the plan and the apply, asked once per run for a target that serves a
    /// selected selector, after `discover` (a flag the floor raised, a warning the selection earns).
    fn plan_notes(&self, _env: &Env, _selection: &Selection) -> Vec<String> {
        Vec::new()
    }

    /// Whether `execute` of `c` ended by leaving its command running in the kernel because the run was
    /// interrupted (Story 4.3.2). The executor then records the item as `Status::Running` and the run as
    /// interrupted.
    fn left_running(&self, _c: &Candidate) -> bool {
        false
    }

    /// What `execute` of `c` reported as trimmed (`fstrim`), recorded as `trimmed_bytes` and never as freed
    /// (Story 4.3.3, UX-14).
    fn trimmed_bytes(&self, _c: &Candidate) -> Option<u64> {
        None
    }

    /// The size of the duplicate set `c` belongs to, for the `kept as newest of N duplicates` text.
    fn duplicate_set_size(&self, _c: &Candidate) -> Option<usize> {
        None
    }

    /// Post-kind hook for whole-kind reporting; returns notes for the report.
    fn finish(&self, _outcomes: &[Outcome], _env: &Env) -> Vec<String> {
        Vec::new()
    }
}

/// Execution order: Cache, Worktree, NodeModules and CargoTarget, Steam, Docker, Btrfs.
pub fn kind_order(kind: Kind) -> u8 {
    match kind {
        Kind::Cache => 0,
        Kind::Worktree => 1,
        Kind::NodeModules | Kind::CargoTarget => 2,
        Kind::SteamPrefix | Kind::SteamShaderCache | Kind::SteamRemnant => 3,
        Kind::DockerImage | Kind::DockerBuildCache | Kind::DockerVolume => 4,
        Kind::BtrfsBalance | Kind::BtrfsTrim => 5,
        #[cfg(feature = "test-support")]
        Kind::FixtureStale => 2,
    }
}

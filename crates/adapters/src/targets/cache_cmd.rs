//! Command-driven caches (Story 2.3.2): `go clean -cache`, `pip cache purge`, `brew cleanup`,
//! `xcrun simctl delete unavailable` and `apt-get clean`. The tool decides what it removes, so the freed
//! bytes are measured as the walk of the cache directory before the command minus the walk after it, with
//! the one tree-walk rule of `treestat::measure`, never read from the tool's own output.
//!
//! Planning is pure over the two read-only ports (`exists`, `euid`); only [`run_cache_command`] mutates, and
//! only through `CommandRunner::run_mutating`, which needs the `ApplyToken` and the allowlist row.

use std::path::Path;

use disk_cleanup_core::cache_targets::{root_decision, CacheCommand};
use disk_cleanup_core::ports::{Cmd, CommandRunner, SystemProbe};
use disk_cleanup_core::{
    AllocatedBytes, ApparentBytes, ApplyToken, DeleteFailure, Freed, Identity, RootDev, SkipReason,
};

use crate::dirops::{open_verified_root, DirOps};
use crate::treestat::{measure, MeasureOpts, TreeStats};

/// What planning needs from the world.
pub struct CmdContext<'a> {
    pub runner: &'a dyn CommandRunner,
    pub probe: &'a dyn SystemProbe,
    /// The caller named the root or home explicitly (`--root`), which lifts the root-run restriction.
    pub explicit_root: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyCmd {
    pub cmd: Cmd,
    pub program: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdPlan {
    Ready(ReadyCmd),
    Skip {
        reason: SkipReason,
        /// `pip3 not installed` for `ToolMissing`.
        note: Option<String>,
    },
}

/// Decide whether `spec` can run now and with which program. The euid rules come first, so a non-root user
/// is told `NeedsRoot` for apt whether or not `apt-get` is installed; a missing tool names the last program
/// tried (`pip3`).
pub fn plan_command(spec: &CacheCommand, needs_root: bool, ctx: &CmdContext<'_>) -> CmdPlan {
    if let Some(reason) = root_decision(needs_root, ctx.probe.euid(), ctx.explicit_root) {
        return CmdPlan::Skip { reason, note: None };
    }
    match spec.programs.iter().find(|p| ctx.runner.exists(p)) {
        Some(program) => CmdPlan::Ready(ReadyCmd {
            cmd: Cmd::new(*program, spec.args.iter().copied(), true),
            program: (*program).to_string(),
        }),
        None => CmdPlan::Skip {
            reason: SkipReason::ToolMissing,
            note: spec.programs.last().map(|p| format!("{p} not installed")),
        },
    }
}

fn stats_of<D: DirOps>(ops: &D, dir: &Path, root_dev: RootDev) -> Option<TreeStats> {
    let md = std::fs::symlink_metadata(dir).ok()?;
    let root = open_verified_root(ops, dir, Identity::of(&md)).ok()?;
    Some(measure(ops, root.dir(), root_dev, MeasureOpts::default()))
}

fn delta(before: &TreeStats, after: Option<&TreeStats>) -> Freed {
    let (alloc, app, entries, inodes) = after.map_or((0, 0, 0, 0), |a| {
        (a.allocated.0, a.apparent.0, a.entries, a.inodes)
    });
    Freed {
        attributed_bytes: AllocatedBytes(before.allocated.0.saturating_sub(alloc)),
        apparent_bytes: ApparentBytes(before.apparent.0.saturating_sub(app)),
        entries_removed: before.entries.saturating_sub(entries),
        inodes_freed: before.inodes.saturating_sub(inodes),
    }
}

/// Run `cmd` and report what left `dir`. `expect` is the identity recorded at discovery: a directory that was
/// replaced since is `IdentityMismatch` and the command does not run. When the command fails or times out the
/// `DeleteFailure` still carries whatever the directory lost.
pub fn run_cache_command<D: DirOps>(
    ops: &D,
    dir: &Path,
    expect: Identity,
    root_dev: RootDev,
    cmd: &Cmd,
    runner: &dyn CommandRunner,
    proof: &ApplyToken,
) -> Result<Freed, DeleteFailure> {
    let fail = |error| DeleteFailure {
        error,
        freed: Freed::default(),
    };
    let root = open_verified_root(ops, dir, expect).map_err(fail)?;
    let before = measure(ops, root.dir(), root_dev, MeasureOpts::default());
    drop(root);
    let ran = runner.run_mutating(cmd, proof);
    let after = stats_of(ops, dir, root_dev);
    let freed = delta(&before, after.as_ref());
    match ran {
        Ok(_) => Ok(freed),
        Err(error) => Err(DeleteFailure { error, freed }),
    }
}

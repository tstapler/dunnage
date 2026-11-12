//! The in-session reviewed plan (Task 6.6.3e, ADR-006 A5 and A6): what `p` produces and `d` consumes.
//!
//! [`review_marks`] plans the marks as the kind `explorer-selection`, saves the plan in `last_plan` and returns
//! a [`ReviewedPlan`] holding the exact [`ItemIdentity`](disk_cleanup_core::plan::ItemIdentity) of every item,
//! each scan root's `(dev, ino)` and the plan-id. The session keeps that value in memory; `Confirm::new`
//! consumes it, `Confirm::accepts` turns it into a `ConfirmedPlan`, and the delete takes only that. A matching
//! `last_plan` entry alone never authorises a delete: a restart plans again (review C10).

use std::path::PathBuf;
use std::sync::Arc;

use disk_cleanup_adapters::treestat::TreestatSizer;
use disk_cleanup_adapters::RealDirOps;
use disk_cleanup_core::explore::confirm::ReviewedPlan;
use disk_cleanup_core::explore::mark_check::OwnState;
use disk_cleanup_core::explorer_plan::{explorer_plan, ExplorerPlanCtx, NoCleanerView};
use disk_cleanup_core::plan::{ExplorerGateRefusal, OnlyMode};
use disk_cleanup_core::plan_state::SaveOutcome;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::Dirs;

use super::actions::ExplorerPorts;
use crate::wiring::env::read_root;
use crate::wiring::explore::canonical_lenient;

/// What the explorer's review and delete need beyond the planner's ports. The roots are the ones resolved
/// when the explorer started; the apply resolves them again.
pub struct DeleteWiring {
    /// The XDG bases before the tool's own directory is appended.
    pub base: Dirs,
    pub roots: RootSet,
    pub exe: PathBuf,
}

/// Plans `marks` for deletion and returns the reviewed plan. `Err` carries the line for the footer; nothing
/// is deleted either way.
pub fn review_marks(
    ports: &ExplorerPorts,
    wiring: &Arc<DeleteWiring>,
    marks: &[PathBuf],
) -> Result<ReviewedPlan, String> {
    if marks.is_empty() {
        return Err("Nothing is marked. Space marks an item.".to_string());
    }
    let own_state = OwnState::canonical(&wiring.base, wiring.exe.clone(), &canonical_lenient);
    let sizer = TreestatSizer::new(RealDirOps::confined_to(&read_root()));
    let ctx = ExplorerPlanCtx {
        roots: &wiring.roots,
        own_state: &own_state,
        fs: &*ports.fs,
        probe: &*ports.probe,
        clock: &*ports.clock,
        sizer: &sizer,
        extents: None,
        cleaner: &NoCleanerView,
    };
    let plan = explorer_plan(marks, OnlyMode::Explorer, &ctx).map_err(|e| e.text().to_string())?;
    let reviewed = ReviewedPlan::from_explorer_plan(&plan);
    if reviewed.count() == 0 {
        return Err("Nothing in the marked set can be deleted. Nothing was deleted.".to_string());
    }
    let euid = ports.probe.euid();
    let now = ports.clock.now_epoch();
    let saved = ports
        .plans
        .save_merged(vec![plan.saved_selection(euid, now)], euid, now);
    if matches!(saved, SaveOutcome::NotSaved(_)) {
        return Err(ExplorerGateRefusal::NotUsable.text());
    }
    Ok(reviewed)
}

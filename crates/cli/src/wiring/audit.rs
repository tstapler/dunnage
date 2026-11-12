//! Composition root pieces of Story 2.2.1: the baked build provenance, the audit sink and plan store over a
//! state dir.

use std::sync::Arc;

use dunnage_adapters::audit::RealAuditSink;
use dunnage_adapters::state::{RealPlanStore, StateDir};
use dunnage_core::BuildInfo;

/// What `cli/build.rs` baked in. `patched` is always false since the scanner is a workspace member (ADR-007);
/// `metascope_rev` is the last commit that touched `crates/scan`.
pub fn build_info() -> BuildInfo {
    BuildInfo {
        bin_version: env!("CARGO_PKG_VERSION").to_string(),
        git_commit: env!("DC_GIT_COMMIT").to_string(),
        dirty: env!("DC_GIT_DIRTY") == "true",
        metascope_rev: env!("DC_SCAN_REV").to_string(),
        patched: false,
        metascope_dirty: env!("DC_SCAN_DIRTY") == "true",
    }
}

pub fn audit_sink(state: &StateDir) -> RealAuditSink {
    RealAuditSink::new(
        state.paths.audit(),
        state.paths.audit_reserve(),
        state.owner,
    )
}

pub fn plan_store(state: StateDir) -> RealPlanStore {
    let now: Arc<dyn Fn() -> i64 + Send + Sync> = Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    });
    RealPlanStore::new(state, now)
}

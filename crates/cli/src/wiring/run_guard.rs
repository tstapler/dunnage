//! Composition pieces of Story 2.2.2: the run lock over the state dir, the signal handlers of an apply or a
//! plan run, and the `plan during an apply` probe. `main` and `compose_apply` call these; the executor sees
//! only the `RunGuard` port.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dunnage_adapters::lock::{RealRunGuard, RunLock};
use dunnage_adapters::signals::{self, Mode, SignalState};
use dunnage_adapters::state::Owner;

pub fn run_lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join("run.lock")
}

/// Apply mode: installs the signal handlers and returns the `RunGuard` whose flag they set.
pub fn install_apply(state_dir: &Path, owner: Option<Owner>) -> io::Result<RealRunGuard> {
    let state = Arc::new(SignalState::new());
    signals::install(Mode::Apply, Arc::clone(&state), Box::new(io::stderr()))?;
    Ok(RealRunGuard::new(run_lock_path(state_dir), owner, state))
}

/// Plan mode: a signal prints the scan-interrupted line and exits 128 plus the signal, without waiting.
pub fn install_plan() -> io::Result<()> {
    signals::install(
        Mode::Plan,
        Arc::new(SignalState::new()),
        Box::new(io::stderr()),
    )
}

/// The `plan during an apply` note (`design/ux.md` 3.5) when another process holds `run.lock`; plan mode
/// never takes the lock, it only probes and lets go at once.
pub fn plan_during_apply_note(state_dir: &Path) -> Option<String> {
    RunLock::probe(&run_lock_path(state_dir)).map(|pid| {
        format!(
            "note: another dunnage apply is running (pid {pid}); sizes and items below are in flux and a plan-id from this report may be refused as changed once it finishes."
        )
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn plan_note_should_name_the_holder_pid_only_while_the_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(plan_during_apply_note(dir.path()), None);
        assert!(
            !run_lock_path(dir.path()).exists(),
            "a probe creates nothing"
        );
        let held = RunLock::try_acquire(&run_lock_path(dir.path()), None).unwrap();
        let note = plan_during_apply_note(dir.path()).unwrap();
        assert_eq!(
            note,
            format!("note: another dunnage apply is running (pid {}); sizes and items below are in flux and a plan-id from this report may be refused as changed once it finishes.", std::process::id())
        );
        drop(held);
        assert_eq!(plan_during_apply_note(dir.path()), None);
    }
}

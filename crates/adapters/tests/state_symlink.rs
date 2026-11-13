#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unnecessary_wraps,
    non_snake_case
)]
//! Phase 6 finding 1: under sudo the state directory is user-owned while the process is root, so a symlink the
//! user plants at a state file must be refused, not followed (the target would be created, appended to,
//! truncated or chowned). The fixtures plant the link in a temp dir; no root is needed to see the open follow it.

use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::Arc;

use dunnage_adapters::lock::RunLock;
use dunnage_adapters::state::{
    append_outcome, release_reserve, OutcomeLine, OutcomeMode, RealPlanStore, StateDir,
};
use dunnage_core::ports::PlanStore;
use dunnage_core::{DiskVerdict, SavedSelection};

const VICTIM: &str = "victim-content\n";

fn plenty(_: &Path) -> std::io::Result<u64> {
    Ok(1 << 30)
}

/// A temp dir holding a victim file and a helper that links `name` in the same dir to it.
fn setup() -> (tempfile::TempDir, std::path::PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let victim = d.path().join("victim");
    std::fs::write(&victim, VICTIM).unwrap();
    (d, victim)
}

fn victim_untouched(victim: &Path) {
    assert_eq!(std::fs::read_to_string(victim).unwrap(), VICTIM);
}

#[test]
fn run_lock_should_refuse_and_leave_the_target_alone_When_run_lock_is_a_symlink() {
    let (d, victim) = setup();
    symlink(&victim, d.path().join("run.lock")).unwrap();

    let err = RunLock::try_acquire(&d.path().join("run.lock"), None).err();

    assert!(err.is_some(), "a symlinked run.lock must not be locked");
    victim_untouched(&victim);
}

#[test]
fn append_outcome_should_refuse_and_leave_the_target_alone_When_outcome_jsonl_is_a_symlink() {
    let (d, victim) = setup();
    let (sd, _) = StateDir::open(d.path(), 1000, &plenty).unwrap();
    symlink(&victim, sd.paths.outcome()).unwrap();
    let line = OutcomeLine {
        schema: 1,
        ts: 1,
        run_id: "r".into(),
        mode: OutcomeMode::Plan,
        verdict: DiskVerdict::Ok,
        free_bytes: 1,
        unallocated_bytes: None,
        via: None,
    };

    let err = append_outcome(&sd.paths, None, &line).unwrap_err();

    assert!(err.to_string().contains("symbolic link"), "{err}");
    victim_untouched(&victim);
}

#[test]
fn state_dir_should_not_truncate_the_target_When_a_reserve_is_a_symlink() {
    let (d, victim) = setup();
    symlink(&victim, d.path().join("audit.reserve")).unwrap();
    symlink(&victim, d.path().join("last_plan.reserve")).unwrap();

    let (_sd, init) = StateDir::open(d.path(), 1000, &plenty).unwrap();

    assert!(!init.reserves_created);
    victim_untouched(&victim);
    assert!(release_reserve(&d.path().join("audit.reserve")).is_err());
    victim_untouched(&victim);
}

#[test]
fn plan_store_should_not_write_through_a_symlink_When_the_lock_or_tmp_file_is_one() {
    let (d, victim) = setup();
    let (sd, _) = StateDir::open(d.path(), 1000, &plenty).unwrap();
    symlink(&victim, sd.paths.last_plan_lock()).unwrap();
    symlink(&victim, d.path().join("last_plan.tmp")).unwrap();
    let store = RealPlanStore::new(sd, Arc::new(|| 1_800_000_000));

    let _ = store.save_merged(
        vec![SavedSelection {
            plan_id: "p".into(),
            saved_at: 0,
            euid: 1000,
            kinds: "build-dirs".into(),
            flags: String::new(),
            items: vec![],
            items_truncated: false,
        }],
        1000,
        1_800_000_000,
    );

    victim_untouched(&victim);
}

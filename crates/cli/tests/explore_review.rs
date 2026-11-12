#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs,
    non_snake_case
)]
//! Task 6.6.3e: `review_marks`, the in-session reviewed plan. It plans the marks as `explorer-selection`,
//! saves the plan in `last_plan` and returns the value the typed count consumes.

mod explore_rig;

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;

use disk_cleanup_adapters::RealProbe;
use disk_cleanup_cli::explore::actions::ExplorerPorts;
use disk_cleanup_cli::explore::handle::{review_marks, DeleteWiring};
use disk_cleanup_cli::wiring::env::RealFs;
use disk_cleanup_core::plan::PlanId;
use disk_cleanup_core::ports::PlanStore;
use disk_cleanup_core::testing::{
    FakeAuditSink, FakeClock, FakeCommandRunner, FakePlanStore, FakeRunGuard, FakeSpaceMeter,
};
use disk_cleanup_core::{BuildInfo, RunRoots};
use explore_rig::{Ex, NOW};

fn ports_over(ex: &Ex, plans: Arc<FakePlanStore>) -> ExplorerPorts {
    ExplorerPorts {
        fs: Arc::new(RealFs),
        runner: Arc::new(FakeCommandRunner::new()),
        clock: Arc::new(FakeClock::at(NOW)),
        probe: Arc::new(RealProbe),
        space: Arc::new(FakeSpaceMeter::new(
            vec![FakeSpaceMeter::plain_snapshot("/x", 500, 1000)],
            FakeSpaceMeter::stable(),
        )),
        audit: Arc::new(FakeAuditSink::new()),
        guard: Arc::new(FakeRunGuard::new()),
        plans: plans as Arc<dyn PlanStore>,
        dirs: ex.dirs.clone(),
        targets: Vec::new(),
        build: BuildInfo {
            bin_version: "0.1.0".into(),
            git_commit: "abc".into(),
            dirty: false,
            metascope_rev: "none".into(),
            patched: false,
            metascope_dirty: false,
        },
        roots: RunRoots::default(),
        run_id: "run-1".into(),
        jobs: 1,
    }
}

fn wiring(ex: &Ex) -> Arc<DeleteWiring> {
    Arc::new(DeleteWiring {
        base: ex.base_dirs.clone(),
        roots: ex.roots(),
        exe: ex.p("bin/disk-cleanup"),
    })
}

#[test]
fn review_should_return_the_planned_identities_and_save_the_plan_for_the_gate() {
    let ex = Ex::new();
    let a = ex.tree("app/dist", 2, 3000);
    let b = ex.tree("app/out", 1, 3000);
    let plans = Arc::new(FakePlanStore::new());
    let reviewed = review_marks(
        &ports_over(&ex, Arc::clone(&plans)),
        &wiring(&ex),
        &[a.clone(), b.clone()],
    )
    .unwrap();
    assert_eq!(reviewed.count(), 2);
    assert!(reviewed.bytes() > 0);
    assert_eq!(reviewed.roots().len(), 1);
    for p in [&a, &b] {
        let item = reviewed.item(p).unwrap();
        let st = std::fs::symlink_metadata(p).unwrap();
        assert_eq!((item.identity.dev, item.identity.ino), (st.dev(), st.ino()));
    }
    // The saved entry carries the same plan-id the typed count will be bound to.
    let saved = plans.load_last().unwrap();
    assert_eq!(saved.entries.len(), 1);
    assert_eq!(saved.entries[0].plan_id, reviewed.plan_id().to_string());
    assert_eq!(saved.entries[0].kinds, "explorer-selection");
    let _: PlanId = reviewed.plan_id();
}

#[test]
fn review_should_refuse_with_the_footer_text_When_nothing_is_marked_or_nothing_can_be_deleted() {
    let ex = Ex::new();
    let ports = ports_over(&ex, Arc::new(FakePlanStore::new()));
    let w = wiring(&ex);
    assert_eq!(
        review_marks(&ports, &w, &[]).unwrap_err(),
        "Nothing is marked. Space marks an item."
    );
    // The scan root itself is not an item: the plan lists a SKIP row and no FREE one.
    let err = review_marks(&ports, &w, &[ex.code.clone()]).unwrap_err();
    assert_eq!(
        err,
        "Nothing in the marked set can be deleted. Nothing was deleted."
    );
}

#[test]
fn review_should_refuse_When_the_plan_cannot_be_saved() {
    let ex = Ex::new();
    let a = ex.tree("app/dist", 1, 100);
    let ports = ports_over(&ex, Arc::new(FakePlanStore::failing(28)));
    let err = review_marks(&ports, &wiring(&ex), &[a]).unwrap_err();
    assert!(err.contains("plan-id NOT usable"), "{err}");
}

#[test]
fn review_should_refuse_an_overlapping_set_and_an_oversized_one() {
    let ex = Ex::new();
    let outer = ex.tree("app/dist", 1, 100);
    let inner = outer.join("sub");
    let ports = ports_over(&ex, Arc::new(FakePlanStore::new()));
    let w = wiring(&ex);
    let err = review_marks(&ports, &w, &[outer, inner]).unwrap_err();
    assert!(err.contains("is inside"), "{err}");
    let many: Vec<PathBuf> = (0..21)
        .map(|i| ex.tree(&format!("many/d{i}"), 0, 0))
        .collect();
    assert!(review_marks(&ports, &w, &many).is_err());
}

#[test]
fn plan_rows_should_explain_a_file_mark_and_say_nothing_extra_for_other_refusals() {
    use disk_cleanup_core::plan::Planned;
    use disk_cleanup_core::SkipReason;
    let ex = Ex::new();
    let dir = ex.tree("app/dist", 1, 100);
    let file = ex.code.join("app/notes.txt");
    std::fs::write(&file, b"n").unwrap();
    let own = disk_cleanup_core::explore::mark_check::OwnState::new(
        &disk_cleanup_core::Dirs {
            state: dir.clone(),
            ..ex.base_dirs.clone()
        },
        ex.p("bin/x"),
    );
    // `dist` is now the tool's own state (a refusal with no note); the file is not a folder.
    let plan = ex.plan_with(
        &[dir.clone(), file.clone()],
        &ex.roots(),
        &ex.real_sizer(),
        &own,
    );
    let row = |p: &PathBuf| plan.rows.iter().find(|r| &r.path == p).unwrap().clone();
    assert_eq!(
        row(&file).planned,
        Planned::WillNot(SkipReason::ProtectedPath)
    );
    assert_eq!(
        row(&file).note.as_deref(),
        Some("not a folder: the explorer deletes folders only")
    );
    assert_eq!(
        row(&dir).planned,
        Planned::WillNot(SkipReason::ProtectedPath)
    );
    assert_eq!(row(&dir).note, None);
}

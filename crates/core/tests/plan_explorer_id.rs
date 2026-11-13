#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.6.2 (Task 6.6.2b): the identity-bound plan-id and the explorer's gate.

#[path = "common/xfs.rs"]
mod xfs;

use std::path::{Path, PathBuf};

use dunnage_core::explorer_plan::{explorer_plan, ExplorerPlan};
use dunnage_core::plan::{
    check_plan_id, ExplorerGateRefusal, ItemIdentity, OnlyMode, Plan, PlanId,
};
use dunnage_core::roots::RootIdentity;
use dunnage_core::selector::{KindSelector, Selection};
use dunnage_core::target::Guarded;
use dunnage_core::{
    AllocatedBytes, BuildDirFacts, BuildDirFlavor, Candidate, FileKind, Identity, KindFacts,
    Locator, SaveOutcome, SavedPlan, SelectorId,
};
use xfs::{none, with_ctx, World, NOW};

fn pool(w: &World) -> Vec<PathBuf> {
    (0..8)
        .map(|i| {
            let p = format!("/home/u/code/m{i}");
            w.fs.dir(&p, 4096 * (i + 1));
            PathBuf::from(p)
        })
        .collect()
}

fn plan_of(w: &World, marks: &[PathBuf]) -> ExplorerPlan {
    with_ctx(w, &none(), |ctx| {
        explorer_plan(marks, OnlyMode::Explorer, ctx).unwrap()
    })
}

#[test]
fn safety_explorer_plan_id_binds_marks_identities_and_kind_When_any_of_them_changes() {
    let w = World::new();
    let marks = pool(&w);
    let id = |m: &[PathBuf]| plan_of(&w, m).plan_id();

    // Every non-empty subset of eight marks has its own id (exhaustive, so deterministic).
    let mut seen = std::collections::HashMap::new();
    for mask in 1u32..256 {
        let set: Vec<PathBuf> = (0..8)
            .filter(|b| mask & (1 << b) != 0)
            .map(|b| marks[b].clone())
            .collect();
        if let Some(prev) = seen.insert(id(&set), mask) {
            panic!("masks {prev:#b} and {mask:#b} share a plan-id");
        }
    }

    // Same set, other order: same id.
    let mut rev = marks.clone();
    rev.reverse();
    assert_eq!(id(&marks), id(&rev));

    // The same name with a new inode: another id.
    let before = id(&marks);
    w.fs.replace("/home/u/code/m3");
    assert_ne!(before, id(&marks));

    // A sibling that changes size only (not identity) leaves the id alone.
    let again = id(&marks);
    assert_eq!(again, id(&marks));

    // A vanished mark is not effective: the id changes, so the review is repeated.
    w.fs.remove("/home/u/code/m0");
    assert_ne!(again, id(&marks));
}

#[test]
fn plan_id_should_change_with_each_identity_field_the_root_list_and_the_kind_When_one_input_differs(
) {
    let base = ItemIdentity {
        dev: 1,
        ino: 7,
        kind: FileKind::Dir,
        btime: None,
    };
    let path = Path::new("/home/u/code/a");
    let root = |p: &str, ino: u64| RootIdentity {
        path: PathBuf::from(p),
        ident: Identity { dev: 1, ino },
    };
    let roots = vec![root("/home/u/code", 5)];
    let id = |i: ItemIdentity, r: &[RootIdentity]| PlanId::for_explorer([(path, i)], r);
    let reference = id(base, &roots);
    for changed in [
        ItemIdentity { dev: 2, ..base },
        ItemIdentity { ino: 8, ..base },
        ItemIdentity {
            kind: FileKind::File,
            ..base
        },
    ] {
        assert_ne!(reference, id(changed, &roots), "{changed:?}");
    }
    assert_ne!(reference, id(base, &[root("/home/u/dotfiles", 5)]));
    assert_ne!(
        reference,
        id(base, &[root("/home/u/code", 5), root("/x", 9)])
    );
    // Review C8: the same root path with another directory behind it (swapped during the session).
    assert_ne!(reference, id(base, &[root("/home/u/code", 6)]));
    // Not the cleaner's id over the same path: the kind is in the hash.
    let cleaner = Candidate::new(
        Locator::Path(path.into()),
        KindFacts::BuildDir(BuildDirFacts {
            flavor: BuildDirFlavor::NodeModules,
            group: "/home/u/code".into(),
            age_secs: 99_999,
            lockfile_hash: None,
        }),
        AllocatedBytes(1),
        Some(1),
        Identity { dev: 1, ino: 7 },
    );
    let mut sel = Selection::new(
        KindSelector::of(&[SelectorId::BuildDirs]),
        KindSelector::default(),
    );
    sel.only = vec![path.into()];
    let plan = Plan::build(&sel, NOW, vec![cleaner], &|_| Guarded::Allow);
    assert_ne!(PlanId::for_selection(&plan, &sel), reference);
}

fn two_marks() -> (World, Vec<PathBuf>) {
    let w = World::new();
    let m = pool(&w)[..2].to_vec();
    (w, m)
}

fn saved(plan: &ExplorerPlan, euid: u32, at: i64) -> SavedPlan {
    SavedPlan {
        entries: vec![plan.saved_selection(euid, at)],
    }
}

#[test]
fn check_plan_id_should_accept_a_fresh_own_entry_and_refuse_stale_foreign_absent_changed_and_unsaved(
) {
    let (w, marks) = two_marks();
    let plan = plan_of(&w, &marks);
    let id = plan.plan_id();
    let ok = |s: Option<&SavedPlan>, euid, now, rec: PlanId, how| {
        check_plan_id(&plan.selection, id, how, s, euid, rec, now)
    };
    let sp = saved(&plan, 1000, NOW);
    let saved_ok = SaveOutcome::Saved;
    assert_eq!(ok(Some(&sp), 1000, NOW + 3600, id, saved_ok), Ok(()));
    // The 24 h boundary is included, one second more is stale.
    assert_eq!(ok(Some(&sp), 1000, NOW + 86_400, id, saved_ok), Ok(()));
    assert_eq!(
        ok(Some(&sp), 1000, NOW + 86_401, id, saved_ok),
        Err(ExplorerGateRefusal::Stale { age_secs: 86_401 })
    );
    // Another user's entry, nothing saved at all.
    assert_eq!(
        ok(Some(&sp), 1001, NOW, id, saved_ok),
        Err(ExplorerGateRefusal::NoSavedPlan)
    );
    assert_eq!(
        ok(None, 1000, NOW, id, saved_ok),
        Err(ExplorerGateRefusal::NoSavedPlan)
    );
    // The marks changed under the id: the recomputed id differs.
    w.fs.replace("/home/u/code/m1");
    let now_plan = plan_of(&w, &marks);
    assert_ne!(now_plan.plan_id(), id);
    assert_eq!(
        ok(Some(&sp), 1000, NOW, now_plan.plan_id(), saved_ok),
        Err(ExplorerGateRefusal::Changed)
    );
    // A plan that could not be saved authorises nothing, even with a matching entry.
    assert_eq!(
        ok(Some(&sp), 1000, NOW, id, SaveOutcome::NotSaved(28)),
        Err(ExplorerGateRefusal::NotUsable)
    );
    for r in [
        ExplorerGateRefusal::NoSavedPlan,
        ExplorerGateRefusal::Changed,
        ExplorerGateRefusal::NotUsable,
        ExplorerGateRefusal::Stale { age_secs: 90_000 },
    ] {
        let t = r.text();
        assert!(t.ends_with("Nothing was deleted."), "{t}");
        if !matches!(r, ExplorerGateRefusal::NotUsable) {
            assert!(t.contains("Review the plan, then press d again."), "{t}");
        }
    }
}

#[test]
fn check_plan_id_should_refuse_a_forged_entry_with_the_right_id_and_another_only_list() {
    let (w, marks) = two_marks();
    let plan = plan_of(&w, &marks);
    let mut forged = saved(&plan, 1000, NOW);
    forged.entries[0].flags = "--only /home/u/code/other".into();
    assert_eq!(
        check_plan_id(
            &plan.selection,
            plan.plan_id(),
            SaveOutcome::Saved,
            Some(&forged),
            1000,
            plan.plan_id(),
            NOW
        ),
        Err(ExplorerGateRefusal::Changed)
    );
}

#[test]
fn a_cleaner_dry_run_id_should_never_satisfy_the_explorers_check() {
    let (w, marks) = two_marks();
    let plan = plan_of(&w, &marks);
    // A build-dirs entry over the same paths, saved by the cleaner under its own id.
    let mut sel = Selection::new(
        KindSelector::of(&[SelectorId::BuildDirs]),
        KindSelector::default(),
    );
    sel.only = marks.clone();
    let cleaner_id = PlanId::from_hex("deadbeef").unwrap();
    let mut entry = plan.saved_selection(1000, NOW);
    entry.plan_id = cleaner_id.to_string();
    entry.kinds = sel.kinds_resolved();
    entry.flags = sel.flags();
    let sp = SavedPlan {
        entries: vec![entry],
    };
    assert_eq!(
        check_plan_id(
            &plan.selection,
            cleaner_id,
            SaveOutcome::Saved,
            Some(&sp),
            1000,
            plan.plan_id(),
            NOW
        ),
        Err(ExplorerGateRefusal::Changed)
    );
}

#[test]
fn check_plan_id_should_be_the_one_destructive_gate_called_from_one_definition() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../");
    let mut defs = Vec::new();
    let mut stack = vec![
        root.join("core/src"),
        root.join("cli/src"),
        root.join("tui/src"),
    ];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let t = std::fs::read_to_string(&p).unwrap();
                for (name, needle) in [
                    ("gate", "fn destructive_gate("),
                    ("check", "fn check_plan_id("),
                ] {
                    if t.contains(needle) {
                        defs.push((name, p.file_name().unwrap().to_string_lossy().into_owned()));
                    }
                }
            }
        }
    }
    defs.sort();
    assert_eq!(
        defs,
        [
            ("check", "plan.rs".to_string()),
            ("gate", "plan.rs".to_string())
        ]
    );
    let plan_rs = std::fs::read_to_string(root.join("core/src/plan.rs")).unwrap();
    let body = &plan_rs[plan_rs.find("pub fn check_plan_id(").unwrap()..];
    assert!(body.contains("destructive_gate(sel, Some(supplied)"));
}

#[test]
fn check_plan_id_should_treat_a_future_saved_at_as_stale_When_the_clock_stepped_back_C10() {
    let (w, marks) = two_marks();
    let plan = plan_of(&w, &marks);
    let id = plan.plan_id();
    let sp = saved(&plan, 1000, NOW + 3600);
    let r = check_plan_id(
        &plan.selection,
        id,
        SaveOutcome::Saved,
        Some(&sp),
        1000,
        id,
        NOW,
    );
    assert_eq!(r, Err(ExplorerGateRefusal::Stale { age_secs: -3600 }));
}

#[test]
fn safety_explorer_plan_id_binds_the_identity_of_each_scan_root() {
    let (w, marks) = two_marks();
    let before = plan_of(&w, &marks).plan_id();
    w.fs.replace("/home/u/code");
    assert_ne!(before, plan_of(&w, &marks).plan_id());
}

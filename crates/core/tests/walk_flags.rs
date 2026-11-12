#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! `--max-seconds`, `--rescan` and `--no-cache` in the core: they ride on `Selection` without touching the
//! plan-id, and a target's partial scan reaches the report (and warns on an apply).

mod common;

use common::{build_dir, run, selection, FakeTarget, Rig};
use dunnage_core::plan::PlanId;
use dunnage_core::ports::Env;
use dunnage_core::selector::Selection;
use dunnage_core::target::{Guarded, KindGate, Target};
use dunnage_core::{ApplyToken, Candidate, DeleteFailure, Freed, SelectorId};

/// A walking target that reports it was cut short.
struct Cut {
    inner: FakeTarget,
    partial: Option<u64>,
}

impl Target for Cut {
    fn name(&self) -> &'static str {
        "cut"
    }
    fn selectors(&self) -> &'static [SelectorId] {
        self.inner.selectors
    }
    fn gate(&self, _env: &Env) -> KindGate {
        KindGate::Available
    }
    fn discover(&self, env: &Env, sel: &Selection) -> Vec<Candidate> {
        self.inner.discover(env, sel)
    }
    fn guard(&self, c: &Candidate, env: &Env) -> Guarded {
        self.inner.guard(c, env)
    }
    fn execute(&self, c: &Candidate, env: &Env, p: &ApplyToken) -> Result<Freed, DeleteFailure> {
        self.inner.execute(c, env, p)
    }
    fn partial_scan(&self) -> Option<u64> {
        self.partial
    }
}

fn cut(partial: Option<u64>) -> Cut {
    Cut {
        inner: FakeTarget::new(
            &[SelectorId::BuildDirs],
            vec![build_dir(1, 4096, 70), build_dir(2, 4096, 70)],
        ),
        partial,
    }
}

#[test]
fn the_plan_id_should_ignore_the_scan_flags_When_max_seconds_rescan_and_no_cache_are_set() {
    let rig = Rig::new();
    let t = cut(None);
    let bare = run(
        &rig,
        &[&t],
        &selection(&[SelectorId::BuildDirs]),
        false,
        1,
        None,
    )
    .unwrap();
    let mut s = selection(&[SelectorId::BuildDirs]);
    s.scan.max_seconds = Some(5);
    s.scan.rescan = true;
    s.scan.no_cache = true;
    let flagged = run(&rig, &[&t], &s, false, 1, None).unwrap();
    assert_eq!(flagged.plan_id, bare.plan_id);
    assert_eq!(flagged.plan_id, PlanId::for_selection(&bare.plan, &s));
    assert_eq!(flagged.partial_scan, None);
    assert_eq!(flagged.plan.items.len(), bare.plan.items.len());
}

#[test]
fn a_partial_scan_should_reach_the_report_and_warn_on_an_apply_When_a_target_was_cut() {
    let rig = Rig::new();
    let t = cut(Some(5));
    let s = selection(&[SelectorId::BuildDirs]);
    let plan = run(&rig, &[&t], &s, false, 1, None).unwrap();
    assert_eq!(plan.partial_scan, Some(5));
    assert!(
        !plan.notes.iter().any(|n| n.contains("PARTIAL SCAN")),
        "the plan prints its banner from the field, not a note"
    );

    let env = rig.env();
    let now = env.clock.now_epoch();
    env.plans.save_merged(
        vec![plan.plan.saved_selection(&s, env.probe.euid(), now)],
        env.probe.euid(),
        now,
    );
    let applied = run(&rig, &[&t], &s, true, 1, Some(plan.plan_id));
    let report = match applied {
        Ok(r) => r,
        Err(r) => panic!("apply refused: {}", r.text),
    };
    assert_eq!(report.partial_scan, Some(5));
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.starts_with("WARNING: PARTIAL SCAN (hit --max-seconds 5)")),
        "{:?}",
        report.notes
    );
}

#[test]
fn a_target_that_was_not_selected_should_not_make_the_plan_partial_When_it_was_cut() {
    let rig = Rig::new();
    let t = cut(Some(5));
    let s = selection(&[SelectorId::Caches]);
    let r = run(&rig, &[&t], &s, false, 1, None).unwrap();
    assert_eq!(r.partial_scan, None);
}

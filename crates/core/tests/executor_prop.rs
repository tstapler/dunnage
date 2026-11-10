#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Property oracles of Story 1.1.4 over random plans on the fakes: unsafe fresh facts never reach
//! `execute`, and the books balance (T-104, T-027).

mod common;

use std::collections::HashSet;

use common::{cache, caches_selection, freed, run, FakeTarget, Rig};
use disk_cleanup_core::delete_types::{DeleteError, DeleteFailure};
use disk_cleanup_core::target::Guarded;
use disk_cleanup_core::{SkipReason, Status};
use proptest::prelude::*;

#[derive(Debug, Clone, Copy)]
enum Fresh {
    Allow,
    Dirty,
    Locked,
    OutsideRoot,
    Symlink,
    CrossDevice,
}

#[derive(Debug, Clone, Copy)]
enum Exec {
    Ok(u64),
    Partial(u64),
    NoSpace(u64),
    Scope(u64),
}

fn fresh() -> impl Strategy<Value = Fresh> {
    prop_oneof![
        4 => Just(Fresh::Allow),
        1 => Just(Fresh::Dirty),
        1 => Just(Fresh::Locked),
        1 => Just(Fresh::OutsideRoot),
        1 => Just(Fresh::Symlink),
        1 => Just(Fresh::CrossDevice),
    ]
}

fn exec() -> impl Strategy<Value = Exec> {
    prop_oneof![
        6 => (1u64..1_000_000).prop_map(Exec::Ok),
        2 => (0u64..1_000_000).prop_map(Exec::Partial),
        1 => (0u64..1_000_000).prop_map(Exec::NoSpace),
        1 => (0u64..1_000_000).prop_map(Exec::Scope),
    ]
}

fn item() -> impl Strategy<Value = (bool, Fresh, Exec)> {
    (any::<bool>(), fresh(), exec())
}

fn skip_of(f: Fresh) -> Guarded {
    match f {
        Fresh::Allow => Guarded::Allow,
        Fresh::Dirty => Guarded::Skip(SkipReason::Dirty),
        Fresh::Locked => Guarded::Skip(SkipReason::Locked),
        Fresh::OutsideRoot => Guarded::Skip(SkipReason::OutsideRoot),
        Fresh::Symlink => Guarded::Skip(SkipReason::Symlink),
        Fresh::CrossDevice => Guarded::Skip(SkipReason::CrossDevice),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn executor_should_never_execute_item_whose_fresh_facts_are_dirty_locked_or_outside_root_When_random_plans_apply(
        items in proptest::collection::vec(item(), 1..24),
        jobs in 1usize..5,
    ) {
        let rig = Rig::new();
        let cands: Vec<_> = (0..items.len()).map(|i| cache(i as u64 + 1, 4096)).collect();
        let script = items.clone();
        let mut t = FakeTarget::caches(cands.clone());
        // plan-time guard: the first coin; execute-time guard: the random fresh facts
        t.guard = Box::new(move |c, n| {
            let (plan_ok, fresh, _) = script[c.ident().ino as usize - 1];
            if n == 0 {
                if plan_ok { Guarded::Allow } else { Guarded::Skip(SkipReason::TooRecent) }
            } else {
                skip_of(fresh)
            }
        });
        let script = items.clone();
        t.exec = Box::new(move |c| match script[c.ident().ino as usize - 1].2 {
            Exec::Ok(n) => Ok(freed(n)),
            Exec::Partial(n) => Err(DeleteFailure { error: DeleteError::Io(13), freed: freed(n) }),
            Exec::NoSpace(n) => Err(DeleteFailure { error: DeleteError::NoSpace, freed: freed(n) }),
            Exec::Scope(n) => Err(DeleteFailure { error: DeleteError::IdentityMismatch, freed: freed(n) }),
        });
        let r = run(&rig, &[&t], &caches_selection(), true, jobs, None).unwrap();
        let unsafe_ids: HashSet<_> = cands
            .iter()
            .zip(&items)
            .filter(|(_, (plan_ok, fresh, _))| !plan_ok || !matches!(fresh, Fresh::Allow))
            .map(|(c, _)| c.id())
            .collect();
        for id in t.executed() {
            prop_assert!(!unsafe_ids.contains(&id), "an item with unsafe fresh facts reached execute");
        }
        // T-027: books balance, failed ones included
        let sum: u64 = r.summary.outcomes.iter().map(|o| o.freed.attributed_bytes.0).sum();
        prop_assert_eq!(r.summary.attributed_bytes, sum);
        prop_assert_eq!(r.summary.outcomes.len(), items.len());
        for o in &r.summary.outcomes {
            if let Status::Skipped(_) = o.status {
                if !t.executed().contains(&o.id) {
                    prop_assert_eq!(o.freed.attributed_bytes.0, 0);
                }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn attributed_bytes_should_equal_sum_of_outcome_freed_When_random_plans_run_on_fakes(
        results in proptest::collection::vec(exec(), 1..30),
        jobs in 1usize..5,
    ) {
        let rig = Rig::new();
        let script = results.clone();
        let cands: Vec<_> = (0..results.len()).map(|i| cache(i as u64 + 1, 4096)).collect();
        let mut t = FakeTarget::caches(cands);
        t.exec = Box::new(move |c| match script[c.ident().ino as usize - 1] {
            Exec::Ok(n) => Ok(freed(n)),
            Exec::Partial(n) => Err(DeleteFailure { error: DeleteError::Io(5), freed: freed(n) }),
            Exec::NoSpace(n) => Err(DeleteFailure { error: DeleteError::NoSpace, freed: freed(n) }),
            Exec::Scope(n) => Err(DeleteFailure { error: DeleteError::Symlink, freed: freed(n) }),
        });
        let r = run(&rig, &[&t], &caches_selection(), true, jobs, None).unwrap();
        let sum: u64 = r.summary.outcomes.iter().map(|o| o.freed.attributed_bytes.0).sum();
        prop_assert_eq!(r.summary.attributed_bytes, sum);
        let executed: u64 = t
            .executed()
            .iter()
            .map(|id| {
                let i = r.summary.outcomes.iter().position(|o| o.id == *id).unwrap();
                match results[i] {
                    Exec::Ok(n) | Exec::Partial(n) | Exec::NoSpace(n) | Exec::Scope(n) => n,
                }
            })
            .sum();
        prop_assert_eq!(r.summary.attributed_bytes, executed, "no lost and no double-counted item");
    }
}

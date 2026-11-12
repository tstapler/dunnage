#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Task 6.6.3a: a `ReviewedPlan` made from an `ExplorerPlan` keeps its plan-id, count and bytes.

#[path = "common/xfs.rs"]
mod xfs;

use std::path::PathBuf;

use dunnage_core::explore::confirm::{Confirm, ReviewedPlan};
use dunnage_core::explorer_plan::explorer_plan;
use dunnage_core::plan::OnlyMode;
use xfs::{none, with_ctx, World};

#[test]
fn from_explorer_plan_should_keep_the_plan_id_count_and_bytes_of_the_rows_that_would_be_removed() {
    let w = World::new();
    let mut marks: Vec<PathBuf> = Vec::new();
    for i in 0..3 {
        let p = format!("/home/u/code/m{i}");
        w.fs.dir(&p, 4096 * (i + 1));
        marks.push(p.into());
    }
    let plan = with_ctx(&w, &none(), |ctx| {
        explorer_plan(&marks, OnlyMode::Explorer, ctx).unwrap()
    });
    let reviewed = ReviewedPlan::from_explorer_plan(&plan);
    assert_eq!(reviewed.plan_id(), plan.plan_id());
    assert_eq!(reviewed.count(), plan.will_count());
    assert_eq!(reviewed.count(), 3);
    assert_eq!(reviewed.bytes(), plan.total_bytes());
    assert_eq!(reviewed.roots(), plan.roots.as_slice());
    let confirmed = Confirm::new(reviewed).accepts("3").unwrap();
    assert_eq!(confirmed.plan_id(), plan.plan_id());
}

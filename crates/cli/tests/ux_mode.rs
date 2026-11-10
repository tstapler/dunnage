#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.2.3: the mode markers (UX-1, UX-2).

mod support;

use disk_cleanup_cli::apply::render_apply;
use disk_cleanup_cli::render::render_plan;
use disk_cleanup_cli::view::PlanView;
use support::*;

#[test]
fn first_line_should_distinguish_dry_run_from_applying_and_no_other_line_should_contain_either_word(
) {
    let fx = Fixture::s1();
    let input = fx.input();
    let plan = render_plan(&input, &PlanView::build(&input), &pipe(80));
    let apply = render_apply(&RunFx::s3().input(1), &pipe(80));
    assert!(plan.lines().next().unwrap().starts_with("DRY RUN"));
    assert!(apply.lines().next().unwrap().starts_with("APPLYING"));
    assert_eq!(plan.matches("APPLYING").count(), 0);
    assert_eq!(apply.matches("DRY RUN").count(), 0);
    let dry: Vec<&str> = plan.lines().filter(|l| l.contains("DRY RUN")).collect();
    assert_eq!(dry.len(), 2, "first and last line only: {dry:?}");
    let applying: Vec<&str> = apply.lines().filter(|l| l.contains("APPLYING")).collect();
    assert_eq!(applying.len(), 1);
}

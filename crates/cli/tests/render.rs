#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.2.3: filesystem block and tier column (T-264, T-265), the `status` and `kinds` screens.

mod support;

use dunnage_cli::render::{render_kinds, render_plan, render_status};
use dunnage_cli::render_json::kind_rows;
use dunnage_cli::view::PlanView;
use support::*;

#[test]
fn report_should_print_df_and_btrfs_allocation_block_When_any_run_plan_or_apply_or_status() {
    let fx = Fixture::s1();
    let input = fx.input();
    let plan = render_plan(&input, &PlanView::build(&input), &pipe(80));
    for key in ["df ", "data ", "metadata ", "unallocated "] {
        assert!(
            plan.lines().any(|l| l.trim_start().starts_with(key)),
            "{key}\n{plan}"
        );
    }
    let status = render_status(&fs_btrfs(), Some("/home/tstapler".as_ref()), &pipe(80));
    assert!(
        status.starts_with("Filesystem /  (btrfs)    verdict: WARN\n"),
        "{status}"
    );
    assert!(
        status.contains("  unallocated    9.8 GiB\n")
            && status.ends_with("lists what could be freed.\n")
    );
    let apply = dunnage_cli::apply::render_apply(&RunFx::s3().input(0), &pipe(80));
    assert!(apply.contains("BEFORE") && apply.contains("AFTER") && apply.contains("CHANGE"));
    assert!(apply.contains("-75.4 GiB") && apply.contains("+75.4 GiB"));
}

#[test]
fn tier_column_should_be_dropped_below_90_columns_and_unbuilt_kinds_line_printed_When_gate_B_build()
{
    let fx = Fixture::s1();
    let mut input = fx.input();
    input.not_built = [
        "worktrees",
        "build-dirs",
        "docker",
        "docker-images",
        "docker-volumes",
        "steam",
        "btrfs",
        "btrfs-trim",
    ]
    .map(String::from)
    .to_vec();
    let at = |w: usize| render_plan(&input, &PlanView::build(&input), &pipe(w));
    assert!(at(80)
        .lines()
        .find(|l| l.starts_with("STATUS"))
        .is_some_and(|h| !h.contains("TIER")));
    let wide = at(100);
    assert!(wide
        .lines()
        .find(|l| l.starts_with("STATUS"))
        .is_some_and(|h| h.contains("TIER")));
    assert!(
        wide.lines().any(|l| l.contains("destructive"))
            && wide.lines().any(|l| l.contains("guarded"))
            && wide.lines().any(|l| l.contains(" safe "))
    );
    let notes: String = at(80)
        .split("Notes:")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(notes.contains("not available in this build: worktrees, build-dirs, docker, docker-images, docker-volumes, steam, btrfs, btrfs-trim"), "{notes}");
}

#[test]
fn kinds_table_should_list_every_selector_with_its_available_cell() {
    let rows = kind_rows(&|s| {
        if s.id() == "steam" {
            "not available in this build".into()
        } else {
            "yes".into()
        }
    });
    let out = render_kinds(&rows, &pipe(80));
    assert_eq!(out.lines().count(), 11);
    assert!(out.lines().next().unwrap().starts_with("ID"));
    assert!(out.lines().any(|l| l.starts_with("steam")
        && l.contains("not available in this build")
        && l.contains("destructive")));
}

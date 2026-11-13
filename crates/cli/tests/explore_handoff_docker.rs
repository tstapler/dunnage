#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Phase 6 finding 3: a tier whose rows are all Docker objects has no path to name, and an empty `--only`
//! selects the whole kind, so the explorer's hand-off must print no line for it.

mod support;

use dunnage_cli::explore::handoff::tier_commands;
use dunnage_core::plan::{Plan, PlanItem, Planned};
use dunnage_core::target::Guarded;
use dunnage_core::SelectorId;
use support::{docker_dangling, docker_volumes, selection_of, NOW};

fn item(c: dunnage_core::Candidate) -> PlanItem {
    PlanItem {
        candidate: c,
        guard: Guarded::Allow,
        planned: Planned::Will,
    }
}

#[test]
fn tier_commands_should_emit_no_line_When_the_tier_holds_only_docker_rows() {
    let plan = Plan {
        items: vec![item(docker_dangling(1.0)), item(docker_volumes(2.0))],
        now: NOW,
    };
    let sel = selection_of(&[SelectorId::Docker, SelectorId::DockerVolumes]);
    let lines = tier_commands(&plan, &sel);
    assert!(
        lines.is_empty(),
        "docker-only tiers must not produce a hand-off line, got {:?}",
        lines.iter().map(|l| &l.command).collect::<Vec<_>>()
    );
}

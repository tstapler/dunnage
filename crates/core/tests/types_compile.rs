#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! The `compile_fail` half of these checks lives in doctests (`cargo test --doc`) on `Locator`,
//! `KindFacts`, `Candidate`, `CandidateId`, `Allowed` and `WorktreeRemoval`; this file holds the runtime half.

use dunnage_core::*;

#[test]
fn locator_should_make_both_none_or_both_some_unrepresentable_and_cache_kind_with_docker_facts_should_not_compile_When_doctests(
) {
    // Exhaustive with no wildcard: a fourth locator shape, or a "neither" shape, breaks this match.
    fn place(l: &Locator) -> &'static str {
        match l {
            Locator::Path(_) => "path",
            Locator::Docker(_) => "docker",
            Locator::Steam(_, _) => "steam",
        }
    }
    assert_eq!(place(&Locator::Path("/p".into())), "path");
    assert_eq!(place(&Locator::Docker(DockerId("d".into()))), "docker");
    assert_eq!(place(&Locator::Steam(AppId(1), "/s".into())), "steam");

    // The kind comes from the facts, so docker facts always give a docker kind.
    let docker = KindFacts::Docker(DockerFacts::BuildCache);
    assert_eq!(docker.kind(), Kind::DockerBuildCache);
    assert_ne!(docker.kind(), Kind::Cache);
}

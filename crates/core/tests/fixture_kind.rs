#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 2.3.4: the plan-id gate decides on the tier of the resolved selector, not on an id, so the test-only
//! `fixture-destructive` kind drives the destructive flow of Gate B exactly as `build-dirs` will at Gate C.

use dunnage_core::plan::{destructive_gate, GateRefusal, PlanId};
use dunnage_core::selector::{KindSelector, Selection};
use dunnage_core::{Risk, SelectorId};

fn sel(ids: &[SelectorId]) -> Selection {
    Selection::new(KindSelector::of(ids), KindSelector::default())
}

#[test]
fn a_destructive_selector_with_another_id_should_be_refused_without_a_plan_id() {
    let fixture = sel(&[SelectorId::FixtureDestructive]);
    assert_eq!(SelectorId::FixtureDestructive.tier(), Risk::Destructive);
    assert!(SelectorId::FixtureDestructive.is_destructive());
    assert!(fixture.needs_plan_id());
    let id = PlanId::from_hex("91be04d7").unwrap();
    let refusal = destructive_gate(&fixture, None, None, id, &[], 1_800_000_000).unwrap_err();
    assert!(
        matches!(refusal, GateRefusal::MissingPlanId { .. }),
        "{refusal:?}"
    );

    // The same gate lets a Safe-tier selection through.
    let caches = sel(&[SelectorId::Caches]);
    assert!(destructive_gate(&caches, None, None, id, &[], 1_800_000_000).is_ok());
}

#[test]
fn the_fixture_kind_should_be_known_to_the_parser_only_when_registered() {
    let parse = |registered: &[SelectorId]| KindSelector::parse("fixture-destructive", registered);
    assert!(parse(&[SelectorId::FixtureDestructive]).is_ok());
    let err = parse(&[SelectorId::Caches]).unwrap_err().to_string();
    assert!(err.contains("not available in this build"), "{err}");
}

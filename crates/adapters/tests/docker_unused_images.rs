#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Unused tagged images behind an age filter (Story 4.1.3; validation T-184, T-185). A `FakeCommandRunner`
//! answers every command; nothing reaches a real daemon.

#[path = "common/docker_scene.rs"]
mod scene;
use scene::*;

use dunnage_adapters::targets::docker_images::DockerImagesTarget;
use dunnage_adapters::targets::docker_unused_images::DockerUnusedImagesTarget;
use dunnage_core::selector::KindSelector;
use dunnage_core::target::{Guarded, Target};
use dunnage_core::testing::{CallKind, FakeCommandRunner};
use dunnage_core::{Candidate, SelectorId, SkipReason, Status};

fn three_tagged() -> Scene {
    let mut s = Scene::default();
    s.images
        .push(Img::tagged("111111111111", "app/used", "1", 90));
    s.images
        .push(Img::tagged("222222222222", "app/recent", "1", 3));
    s.images
        .push(Img::tagged("333333333333", "app/old", "1", 60));
    s.images[0].containers = 1;
    s.containers.push(Ctr {
        id: "c0ffee000001".into(),
        name: "stopped-app".into(),
        mounts: String::new(),
        image: "app/used:1".into(),
        state: "exited".into(),
    });
    s
}

fn sel_images() -> dunnage_core::selector::Selection {
    let mut sel = select(&[SelectorId::DockerImages]);
    sel.min_age_secs = Some(30 * 86_400);
    sel
}

fn find<'a>(cs: &'a [Candidate], id: &str) -> &'a Candidate {
    cs.iter()
        .find(|c| format!("{:?}", c.locator()).contains(id))
        .unwrap()
}

fn mutating(r: &FakeCommandRunner) -> Vec<String> {
    r.calls_of(CallKind::RunMutating)
        .iter()
        .map(dunnage_core::testing::RecordedCall::rendered)
        .collect()
}

#[test]
fn only_60d_unreferenced_image_should_be_removed_When_min_age_30d() {
    let s = three_tagged();
    let runner = s.runner();
    runner.on(&["docker", "image", "rm", "app/old:1"], ok(""));
    let rig = Rig::with(runner, s.fs());
    let env = rig.env();
    let t = DockerUnusedImagesTarget::new(None);
    let sel = sel_images();

    let cands = t.discover(&env, &sel);
    assert_eq!(
        t.guard(find(&cands, "111111111111"), &env),
        Guarded::Skip(SkipReason::ImageReferenced)
    );
    assert_eq!(
        t.guard(find(&cands, "222222222222"), &env),
        Guarded::Skip(SkipReason::TooRecent)
    );
    assert_eq!(t.guard(find(&cands, "333333333333"), &env), Guarded::Allow);

    let r = plan_then_apply(vec![&t], &env, &sel);

    assert_eq!(
        mutating(&rig.runner),
        vec!["docker image rm app/old:1".to_string()]
    );
    let status = |id: &str| status_for(&r, find(&cands, id));
    assert_eq!(status("333333333333"), Status::Removed);
    assert_eq!(
        status("111111111111"),
        Status::Skipped(SkipReason::ImageReferenced)
    );
    assert_eq!(
        status("222222222222"),
        Status::Skipped(SkipReason::TooRecent)
    );
}

#[test]
fn image_used_by_a_container_should_be_referenced_even_when_the_listing_count_is_zero() {
    // `Containers` says 0 but `docker ps -a` names the image: the second signal wins.
    let mut s = three_tagged();
    s.images[0].containers = 0;
    let rig = Rig::new(&s);
    let env = rig.env();
    let t = DockerUnusedImagesTarget::new(None);
    let cands = t.discover(&env, &sel_images());
    assert_eq!(
        t.guard(find(&cands, "111111111111"), &env),
        Guarded::Skip(SkipReason::ImageReferenced)
    );
}

#[test]
fn default_apply_should_not_touch_tagged_images_When_no_kinds() {
    assert!(!KindSelector::default_apply().contains(SelectorId::DockerImages));
    assert!(!KindSelector::default_apply().contains(SelectorId::DockerVolumes));
    // `--kinds docker` (dangling images and cache) never reaches the tagged images either
    let s = three_tagged();
    let rig = Rig::new(&s);
    let env = rig.env();
    let unused = DockerUnusedImagesTarget::new(None);
    let untagged = DockerImagesTarget::new(None);
    let sel = select(&[SelectorId::Docker]);

    let r = plan_then_apply(vec![&unused, &untagged], &env, &sel);

    assert_eq!(
        rig.runner.mutating_calls(),
        0,
        "{:?}",
        mutating(&rig.runner)
    );
    let not_selected = r
        .plan
        .items
        .iter()
        .filter(|i| i.candidate.selector() == SelectorId::DockerImages)
        .count();
    assert_eq!(not_selected, 3);
}

#[test]
fn image_with_two_tags_should_be_removed_tag_by_tag_When_unreferenced_and_old() {
    let mut s = Scene::default();
    s.images
        .push(Img::tagged("444444444444", "app/two", "1", 60));
    s.images
        .push(Img::tagged("444444444444", "app/two", "latest", 60));
    let runner = s.runner();
    runner.on(&["docker", "image", "rm", "app/two:1"], ok(""));
    runner.on(&["docker", "image", "rm", "app/two:latest"], ok(""));
    let rig = Rig::with(runner, s.fs());
    let env = rig.env();
    let t = DockerUnusedImagesTarget::new(None);
    let sel = sel_images();

    let r = plan_then_apply(vec![&t], &env, &sel);

    assert_eq!(
        mutating(&rig.runner),
        vec![
            "docker image rm app/two:1".to_string(),
            "docker image rm app/two:latest".to_string()
        ]
    );
    assert_eq!(r.summary.outcomes.len(), 1);
}

#[test]
fn image_referenced_by_docker_at_delete_time_should_map_to_Skipped_ImageReferenced_not_Error() {
    let mut s = Scene::default();
    s.images
        .push(Img::tagged("555555555555", "app/race", "1", 60));
    let runner = s.runner();
    runner.on(
        &["docker", "image", "rm", "app/race:1"],
        failed("Error response from daemon: conflict: unable to delete 555555555555 (cannot be forced) - image is being used by stopped container 1f2e3d"),
    );
    let rig = Rig::with(runner, s.fs());
    let env = rig.env();
    let t = DockerUnusedImagesTarget::new(None);

    let r = plan_then_apply(vec![&t], &env, &sel_images());

    assert_eq!(
        r.summary.outcomes[0].status,
        Status::Skipped(SkipReason::ImageReferenced)
    );
}

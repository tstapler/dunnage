#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Dangling images and build cache (Story 4.1.2; validation T-030, T-183, T-190) and the locally-built-image
//! rule of Story 4.1.3 (T-502). A `FakeCommandRunner` answers every command; nothing reaches a real daemon.

#[path = "common/docker_scene.rs"]
mod scene;
use scene::*;

use disk_cleanup_adapters::targets::docker_images::{DockerImagesTarget, DAEMON_SIDE_NOTE};
use disk_cleanup_adapters::targets::docker_unused_images::DockerUnusedImagesTarget;
use disk_cleanup_core::executor::ExitClass;
use disk_cleanup_core::target::{Guarded, KindGate, Target};
use disk_cleanup_core::testing::{CallKind, FakeCommandRunner};
use disk_cleanup_core::{ApplyToken, SelectorId, SkipReason, Status};

fn dangling_scene(n: usize) -> Scene {
    let mut s = Scene::default();
    for i in 0..n {
        s.images.push(Img::untagged(&format!("{i:012x}"), "886MB"));
    }
    s.cache.push(("cache1".into(), "5.8GB".into(), false));
    s
}

fn mutating(r: &FakeCommandRunner) -> Vec<String> {
    r.calls_of(CallKind::RunMutating)
        .iter()
        .map(disk_cleanup_core::testing::RecordedCall::rendered)
        .collect()
}

#[test]
fn apply_should_issue_14_image_rm_and_one_builder_prune_and_never_system_prune_When_dangling() {
    let s = dangling_scene(14);
    let runner = s.runner();
    for i in 0..14 {
        runner.on(&["docker", "image", "rm", &format!("{i:012x}")], ok(""));
    }
    runner.on(
        &[
            "docker",
            "builder",
            "prune",
            "--force",
            "--filter",
            "until=24h",
        ],
        ok(""),
    );
    let rig = Rig::with(runner, s.fs());
    let env = rig.env();
    let t = DockerImagesTarget::new(None);

    let r = plan_then_apply(vec![&t], &env, &select(&[SelectorId::Docker]));

    let cmds = mutating(&rig.runner);
    assert_eq!(cmds.len(), 15, "{cmds:?}");
    for i in 0..14 {
        assert!(
            cmds.contains(&format!("docker image rm {i:012x}")),
            "{cmds:?}"
        );
    }
    assert!(cmds.contains(&"docker builder prune --force --filter until=24h".to_string()));
    for c in &cmds {
        for banned in ["system prune", "image prune", "volume prune", "-a"] {
            assert!(!c.contains(banned), "{c}");
        }
    }
    assert!(r
        .summary
        .outcomes
        .iter()
        .all(|o| o.status == Status::Removed));
    assert_eq!(r.exit, ExitClass::Ok);
    assert!(
        r.notes.iter().any(|n| n == DAEMON_SIDE_NOTE),
        "{:?}",
        r.notes
    );
}

#[test]
fn docker_item_should_use_system_df_delta_and_daemon_side_note_When_18_2_to_0_1_gib() {
    let mut s = Scene::default();
    s.cache.push(("big".into(), "18.2GB".into(), false));
    let runner = s.runner_without(&[DF]);
    // discover, then the read before the prune, then the read after it
    runner.on(DF, ok(&s.df())).on(DF, ok(&s.df())).on(
        DF,
        ok(&{
            let mut after = s.clone();
            after.cache = vec![("left".into(), "0.1GB".into(), false)];
            after.df()
        }),
    );
    runner.on(
        &[
            "docker",
            "builder",
            "prune",
            "--force",
            "--filter",
            "until=24h",
        ],
        ok("Total: 18.1GB\n"),
    );
    let rig = Rig::with(runner, s.fs());
    let env = rig.env();
    let t = DockerImagesTarget::new(None);
    let cands = t.discover(&env, &select(&[SelectorId::Docker]));
    assert_eq!(cands.len(), 1);

    let freed = t
        .execute(&cands[0], &env, &ApplyToken::for_tests())
        .unwrap();

    assert_eq!(freed.attributed_bytes.0, 18_100_000_000);
    let notes = t.finish(
        &[disk_cleanup_core::Outcome {
            ts: 0,
            run_id: "r".into(),
            kind: cands[0].kind(),
            selector: cands[0].selector(),
            id: cands[0].id(),
            path: None,
            status: Status::Removed,
            est_bytes: 0,
            measured_bytes: Some(freed.attributed_bytes.0),
            freed,
            elapsed_ms: 0,
        }],
        &env,
    );
    assert_eq!(notes, vec![DAEMON_SIDE_NOTE.to_string()]);
}

#[test]
fn daemon_dying_mid_run_should_yield_CommandFailed_Error_and_continue_other_kinds_When_image_rm_exits_1(
) {
    let s = dangling_scene(6);
    let runner = s.runner();
    let dead = "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?";
    for i in 0..6 {
        let id = format!("{i:012x}");
        let argv = ["docker", "image", "rm", id.as_str()];
        if i < 3 {
            runner.on(&argv, ok(""));
        } else {
            runner.on(&argv, failed(dead));
        }
    }
    runner.on(
        &[
            "docker",
            "builder",
            "prune",
            "--force",
            "--filter",
            "until=24h",
        ],
        ok(""),
    );
    let rig = Rig::with(runner, s.fs());
    let env = rig.env();
    let t = DockerImagesTarget::new(None);

    let r = plan_then_apply(vec![&t], &env, &select(&[SelectorId::Docker]));

    let statuses: Vec<&Status> = r.summary.outcomes.iter().map(|o| &o.status).collect();
    assert_eq!(
        statuses.iter().filter(|s| ***s == Status::Removed).count(),
        4
    );
    let errors: Vec<&&Status> = statuses
        .iter()
        .filter(|s| matches!(***s, Status::Error(_)))
        .collect();
    assert_eq!(errors.len(), 3);
    for e in errors {
        let Status::Error(m) = e else { unreachable!() };
        assert!(
            m.starts_with("command exited 1: Cannot connect to the Docker daemon"),
            "{m}"
        );
        assert!(!m.contains("errno"), "{m}");
    }
    // the build cache, a different kind, still ran
    assert!(mutating(&rig.runner)
        .contains(&"docker builder prune --force --filter until=24h".to_string()));
    assert_eq!(
        r.abort, None,
        "a dead daemon is an error of the item, not an ENOSPC abort"
    );
    assert_eq!(r.exit, ExitClass::PartialFailure);
}

#[test]
fn referenced_dangling_image_should_not_be_removed_When_a_container_uses_it() {
    let mut s = dangling_scene(2);
    s.images[0].containers = 1;
    let rig = Rig::new(&s);
    let env = rig.env();
    let t = DockerImagesTarget::new(None);
    let cands = t.discover(&env, &select(&[SelectorId::Docker]));
    let used = cands
        .iter()
        .find(|c| format!("{:?}", c.locator()).contains("000000000000"))
        .unwrap();
    assert_eq!(
        t.guard(used, &env),
        Guarded::Skip(SkipReason::ImageReferenced)
    );
    assert_eq!(
        t.guard_before_delete(used, &env),
        Guarded::Skip(SkipReason::ImageReferenced)
    );
}

#[test]
fn nothing_should_be_listed_or_removed_When_the_daemon_is_down() {
    let r = FakeCommandRunner::new();
    r.on(CTX, ok("default\n")).on(
        INFO,
        failed("Cannot connect to the Docker daemon at unix:///var/run/docker.sock"),
    );
    let rig = Rig::with(r, scene::Scene::default().fs());
    let env = rig.env();
    let t = DockerImagesTarget::new(None);
    assert_eq!(
        t.gate(&env),
        KindGate::Unavailable(SkipReason::DaemonUnavailable)
    );
    assert!(t
        .unavailable_detail(&env)
        .unwrap()
        .contains("Cannot connect"));
    assert!(t.discover(&env, &select(&[SelectorId::Docker])).is_empty());
    assert_eq!(rig.runner.mutating_calls(), 0);
}

#[test]
fn untested_server_version_should_list_the_image_and_remove_nothing_When_gate_is_ListOnly() {
    let s = dangling_scene(1);
    let runner = s.runner_without(&[VERSION]);
    runner.on(VERSION, ok("30.1.0\n"));
    let rig = Rig::with(runner, s.fs());
    let env = rig.env();
    let t = DockerImagesTarget::new(None);
    let cands = t.discover(&env, &select(&[SelectorId::Docker]));
    assert_eq!(cands.len(), 2);
    for c in &cands {
        assert_eq!(
            t.guard(c, &env),
            Guarded::Skip(SkipReason::DockerVersionUntested)
        );
        assert_eq!(
            t.guard_before_delete(c, &env),
            Guarded::Skip(SkipReason::DockerVersionUntested)
        );
    }
}

#[test]
fn locally_built_image_with_digest_none_should_be_skipped_LocalOnlyImage_with_a_rmi_next_action() {
    let mut s = Scene::default();
    let mut local = Img::tagged("aaaaaaaaaaaa", "me/built", "dev", 60);
    local.digest = None;
    s.images.push(local);
    s.images
        .push(Img::tagged("bbbbbbbbbbbb", "library/alpine", "3", 60));
    let rig = Rig::new(&s);
    let env = rig.env();
    let t = DockerUnusedImagesTarget::new(None);
    let cands = t.discover(&env, &select(&[SelectorId::DockerImages]));
    let by = |id: &str| {
        cands
            .iter()
            .find(|c| format!("{:?}", c.locator()).contains(id))
            .unwrap()
    };
    assert_eq!(
        t.guard(by("aaaaaaaaaaaa"), &env),
        Guarded::Skip(SkipReason::LocalOnlyImage)
    );
    assert_eq!(t.guard(by("bbbbbbbbbbbb"), &env), Guarded::Allow);
    assert_eq!(
        SkipReason::LocalOnlyImage.next_action(),
        "docker rmi <name> by hand if you are sure"
    );
    assert_eq!(
        t.detail(by("aaaaaaaaaaaa")).as_deref(),
        Some("me/built:dev")
    );
}

#[test]
fn safety_docker_never_issues_a_broad_prune() {
    let s = dangling_scene(3);
    let runner = s.runner();
    for i in 0..3 {
        runner.on(&["docker", "image", "rm", &format!("{i:012x}")], ok(""));
    }
    runner.on(
        &[
            "docker",
            "builder",
            "prune",
            "--force",
            "--filter",
            "until=24h",
        ],
        ok(""),
    );
    let rig = Rig::with(runner, s.fs());
    let env = rig.env();
    let t = DockerImagesTarget::new(None);
    let _ = plan_then_apply(vec![&t], &env, &select(&[SelectorId::Docker]));
    for c in mutating(&rig.runner) {
        assert!(
            c.starts_with("docker image rm ")
                || c == "docker builder prune --force --filter until=24h",
            "{c}"
        );
    }
}

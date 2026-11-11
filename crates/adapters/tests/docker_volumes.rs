#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Unreferenced volumes behind guards (Story 4.1.4; validation T-187 to T-189, T-404, T-186). A
//! `FakeCommandRunner` answers every command and a `FakeFs` holds the mount points; nothing reaches a real
//! daemon or a real volume.

#[path = "common/docker_scene.rs"]
mod scene;
use scene::*;

use disk_cleanup_adapters::targets::docker_images::DockerImagesTarget;
use disk_cleanup_adapters::targets::docker_unused_images::DockerUnusedImagesTarget;
use disk_cleanup_adapters::targets::docker_volumes::DockerVolumesTarget;
use disk_cleanup_core::selector::{DockerVolumes, Selection};
use disk_cleanup_core::target::{Guarded, KindGate, Target};
use disk_cleanup_core::testing::{CallKind, FakeCommandRunner};
use disk_cleanup_core::{Candidate, SelectorId, SkipReason, Status};

fn anon() -> Vol {
    Vol::anon(60)
}

fn scratch() -> Vol {
    Vol::named("scratch", 60)
}

fn sel(class: Option<DockerVolumes>) -> Selection {
    let mut s = select(&[SelectorId::DockerVolumes]);
    s.docker_volumes = class;
    s
}

fn find<'a>(cs: &'a [Candidate], name: &str) -> &'a Candidate {
    cs.iter()
        .find(|c| format!("{:?}", c.locator()).contains(name))
        .unwrap()
}

fn volume_rms(r: &FakeCommandRunner) -> Vec<String> {
    r.calls_of(CallKind::RunMutating)
        .iter()
        .map(disk_cleanup_core::testing::RecordedCall::rendered)
        .collect()
}

fn scene_two() -> (Scene, FakeCommandRunner) {
    let s = Scene {
        volumes: vec![anon(), scratch()],
        ..Scene::default()
    };
    let r = s.runner();
    for v in &s.volumes {
        r.on(&["docker", "volume", "rm", &v.name], ok(""));
    }
    (s, r)
}

#[test]
fn volumes_should_be_OptInRequired_by_default_and_each_class_removable_only_with_its_own_docker_volumes_flag_When_unreferenced_old_small_anonymous_and_named(
) {
    let anon_name = anon().name;
    for (class, removed) in [
        (None, vec![]),
        (
            Some(DockerVolumes::Anonymous),
            vec![format!("docker volume rm {anon_name}")],
        ),
        (
            Some(DockerVolumes::Named),
            vec!["docker volume rm scratch".to_string()],
        ),
    ] {
        let (s, runner) = scene_two();
        let rig = Rig::with(runner, s.fs());
        let env = rig.env();
        let t = DockerVolumesTarget::new(None);

        let r = plan_then_apply(vec![&t], &env, &sel(class));

        assert_eq!(volume_rms(&rig.runner), removed, "{class:?}");
        if class.is_none() {
            assert!(r
                .summary
                .outcomes
                .iter()
                .all(|o| o.status == Status::Skipped(SkipReason::OptInRequired)));
        }
    }
}

#[test]
fn docker_volumes_should_need_own_id_even_When_docker_family_named() {
    let (s, runner) = scene_two();
    let rig = Rig::with(runner, s.fs());
    let env = rig.env();
    let volumes = DockerVolumesTarget::new(None);
    let tagged = DockerUnusedImagesTarget::new(None);
    let untagged = DockerImagesTarget::new(None);

    // `--kinds docker` selects neither volumes nor tagged images, and needs no plan-id
    let docker_only = select(&[SelectorId::Docker]);
    assert!(!docker_only.needs_plan_id());
    let r = plan_then_apply(vec![&volumes, &tagged, &untagged], &env, &docker_only);
    assert_eq!(rig.runner.mutating_calls(), 0);
    assert_eq!(
        r.plan
            .items
            .iter()
            .filter(|i| i.candidate.selector() == SelectorId::DockerVolumes)
            .count(),
        2
    );
    // naming docker-volumes is what asks for the destructive plan-id
    assert!(sel(Some(DockerVolumes::Anonymous)).needs_plan_id());
    assert_eq!(
        sel(None).destructive_selectors(),
        vec![SelectorId::DockerVolumes]
    );
}

#[test]
fn volume_referenced_by_a_stopped_container_or_compose_project_should_be_kept_and_never_rm_d() {
    let mut s = Scene::default();
    let mut pg = Vol::named("pgdata", 90);
    pg.links = 1;
    let mut proj = Vol::named("proj_cache", 90);
    proj.labels = vec![
        ("com.docker.compose.project".into(), "proj".into()),
        (
            "com.docker.compose.project.working_dir".into(),
            "/home/u/code/proj".into(),
        ),
    ];
    let mut gone = Vol::named("old_cache", 90);
    gone.labels = vec![
        ("com.docker.compose.project".into(), "old".into()),
        (
            "com.docker.compose.project.working_dir".into(),
            "/home/u/code/deleted".into(),
        ),
    ];
    s.volumes = vec![pg, proj, gone];
    s.containers.push(Ctr {
        id: "c0ffee000002".into(),
        name: "db".into(),
        mounts: "pgdata".into(),
        image: "postgres:16".into(),
        state: "exited".into(),
    });
    let runner = s.runner();
    runner.on(&["docker", "volume", "rm", "old_cache"], ok(""));
    let mut rig = Rig::with(runner, s.fs());
    rig.fs.add_dir("/home/u/code/proj");
    let env = rig.env();
    let t = DockerVolumesTarget::new(None);
    let sel = sel(Some(DockerVolumes::Named));

    let cands = t.discover(&env, &sel);
    assert_eq!(
        t.guard(find(&cands, "pgdata"), &env),
        Guarded::Skip(SkipReason::VolumeReferenced)
    );
    assert_eq!(t.detail(find(&cands, "pgdata")).as_deref(), Some("db"));
    assert_eq!(
        t.guard(find(&cands, "proj_cache"), &env),
        Guarded::Skip(SkipReason::VolumeReferenced)
    );
    assert_eq!(
        t.detail(find(&cands, "proj_cache")).as_deref(),
        Some("compose project present")
    );
    // a compose label whose working directory is gone does not reference it
    assert_eq!(t.guard(find(&cands, "old_cache"), &env), Guarded::Allow);

    // --volume never lifts a reference
    let mut named = sel;
    named.volumes = vec!["pgdata".into()];
    let cands = t.discover(&env, &named);
    assert_eq!(
        t.guard(find(&cands, "pgdata"), &env),
        Guarded::Skip(SkipReason::VolumeReferenced)
    );
    let r = plan_then_apply(vec![&t], &env, &named);
    assert_eq!(
        volume_rms(&rig.runner),
        vec!["docker volume rm old_cache".to_string()]
    );
    assert_eq!(
        r.summary
            .outcomes
            .iter()
            .filter(|o| o.status == Status::Removed)
            .count(),
        1
    );
}

#[test]
fn volume_created_3_days_ago_should_be_TooRecent_with_the_default_floor() {
    let s = Scene {
        volumes: vec![Vol::anon(3)],
        ..Scene::default()
    };
    let rig = Rig::new(&s);
    let env = rig.env();
    let t = DockerVolumesTarget::new(None);
    let sel = sel(Some(DockerVolumes::Anonymous));
    let cands = t.discover(&env, &sel);
    assert_eq!(
        t.guard(&cands[0], &env),
        Guarded::Skip(SkipReason::TooRecent)
    );
    assert_eq!(rig.runner.mutating_calls(), 0);
}

#[test]
fn database_like_and_unreadable_volumes_should_be_skipped_unless_named_with_volume() {
    let mut pg = Vol::anon(60);
    pg.name = "b".repeat(63) + "2";
    pg.files = Some(vec!["PG_VERSION".into(), "base".into()]);
    let mut big = Vol::anon(60);
    big.name = "c".repeat(63) + "3";
    big.size = "1.2GB".into();
    let mut locked = Vol::anon(60);
    locked.name = "d".repeat(63) + "4";
    locked.files = None;
    let s = Scene {
        volumes: vec![pg.clone(), big.clone(), locked.clone()],
        ..Scene::default()
    };
    let runner = s.runner();
    for v in &s.volumes {
        runner.on(&["docker", "volume", "rm", &v.name], ok(""));
    }
    let rig = Rig::with(runner, s.fs());
    let env = rig.env();
    let t = DockerVolumesTarget::new(None);
    let mut sel = sel(Some(DockerVolumes::Anonymous));

    let cands = t.discover(&env, &sel);
    assert_eq!(
        t.guard(find(&cands, &pg.name), &env),
        Guarded::Skip(SkipReason::LooksLikeDatabase)
    );
    assert_eq!(
        t.detail(find(&cands, &pg.name)).as_deref(),
        Some("PG_VERSION")
    );
    assert_eq!(
        t.guard(find(&cands, &big.name), &env),
        Guarded::Skip(SkipReason::LooksLikeDatabase)
    );
    assert_eq!(
        t.guard(find(&cands, &locked.name), &env),
        Guarded::Skip(SkipReason::ContentsUnknown)
    );

    sel.volumes = vec![locked.name.clone(), pg.name.clone()];
    let cands = t.discover(&env, &sel);
    assert_eq!(t.guard(find(&cands, &pg.name), &env), Guarded::Allow);
    assert_eq!(t.guard(find(&cands, &locked.name), &env), Guarded::Allow);
    assert_eq!(
        t.guard(find(&cands, &big.name), &env),
        Guarded::Skip(SkipReason::LooksLikeDatabase)
    );
}

#[test]
fn volume_rm_refused_by_docker_in_use_should_map_by_message_to_Skipped_VolumeReferenced_not_Error_When_race(
) {
    let s = Scene {
        volumes: vec![scratch(), Vol::named("other", 60)],
        ..Scene::default()
    };
    let runner = s.runner();
    runner.on(
        &["docker", "volume", "rm", "scratch"],
        failed("Error response from daemon: remove scratch: volume is in use - [abc123]"),
    );
    runner.on(
        &["docker", "volume", "rm", "other"],
        failed("Error response from daemon: remove other: permission denied"),
    );
    let rig = Rig::with(runner, s.fs());
    let env = rig.env();
    let t = DockerVolumesTarget::new(None);

    let r = plan_then_apply(vec![&t], &env, &sel(Some(DockerVolumes::Named)));

    let status = |name: &str| {
        let o = r
            .summary
            .outcomes
            .iter()
            .find(|o| {
                r.plan.items.iter().any(|i| {
                    i.candidate.id() == o.id
                        && format!("{:?}", i.candidate.locator()).contains(name)
                })
            })
            .unwrap();
        o.status.clone()
    };
    assert_eq!(
        status("scratch"),
        Status::Skipped(SkipReason::VolumeReferenced)
    );
    match status("other") {
        Status::Error(m) => assert!(m.contains("permission denied"), "{m}"),
        s => panic!("expected an error, got {s:?}"),
    }
}

#[test]
fn container_created_between_plan_and_execute_should_be_seen_by_the_fresh_guard() {
    let s = Scene {
        volumes: vec![scratch()],
        ..Scene::default()
    };
    let runner = s.runner();
    let filter = [
        "docker",
        "ps",
        "-a",
        "--filter",
        "volume=scratch",
        "--format",
        "{{json .}}",
    ];
    // the scene's first answer (no container) is consumed by the first re-check, the next one sees the new
    // container
    runner.on(
        &filter,
        ok(r#"{"ID":"c0ffee000003","Names":"late","Mounts":"scratch","State":"created","Image":"alpine"}"#),
    );
    let rig = Rig::with(runner, s.fs());
    let env = rig.env();
    let t = DockerVolumesTarget::new(None);
    let cands = t.discover(&env, &sel(Some(DockerVolumes::Named)));
    assert_eq!(t.guard(&cands[0], &env), Guarded::Allow);
    assert_eq!(t.guard_before_delete(&cands[0], &env), Guarded::Allow);
    assert_eq!(
        t.guard_before_delete(&cands[0], &env),
        Guarded::Skip(SkipReason::VolumeReferenced)
    );
}

#[test]
fn plan_row_should_show_size_created_at_mount_point_and_class_for_every_listed_volume_When_rendered(
) {
    let s = Scene {
        volumes: vec![anon(), scratch()],
        ..Scene::default()
    };
    let rig = Rig::new(&s);
    let env = rig.env();
    let t = DockerVolumesTarget::new(None);
    let cands = t.discover(&env, &sel(None));
    assert_eq!(cands.len(), 2);
    let a = t.evidence(find(&cands, &anon().name)).unwrap();
    assert!(a.contains("~1.049MB"), "{a}");
    assert!(a.contains("created 2026-"), "{a}");
    assert!(a.contains(&anon().mountpoint()), "{a}");
    assert!(a.ends_with("anonymous"), "{a}");
    let n = t.evidence(find(&cands, "scratch")).unwrap();
    assert!(n.contains("~2.097MB") && n.ends_with("named"), "{n}");
}

#[test]
fn volumes_should_be_Unavailable_and_unlisted_When_ps_and_df_disagree() {
    // an empty `ps -a` while `system df -v` counts containers never reads as "nothing references this volume"
    let mut s = Scene {
        volumes: vec![scratch()],
        ..Scene::default()
    };
    s.containers.push(Ctr {
        id: "c0ffee000004".into(),
        name: "db".into(),
        mounts: "scratch".into(),
        image: "postgres".into(),
        state: "exited".into(),
    });
    let runner = s.runner_without(&[PS]);
    runner.on(PS, ok(""));
    let rig = Rig::with(runner, s.fs());
    let env = rig.env();
    let t = DockerVolumesTarget::new(None);
    assert_eq!(
        t.gate(&env),
        KindGate::Unavailable(SkipReason::UnrecognisedOutput)
    );
    assert!(t
        .discover(&env, &sel(Some(DockerVolumes::Named)))
        .is_empty());
}

#[test]
fn safety_docker_volume_in_use_or_unopted_is_never_removed() {
    // referenced, too recent and not opted in: three independent reasons, none lifted by --volume
    let mut used = Vol::named("pgdata", 90);
    used.links = 1;
    let recent = Vol::named("fresh", 3);
    let s = Scene {
        volumes: vec![used, recent, scratch()],
        containers: vec![Ctr {
            id: "c0ffee000005".into(),
            name: "db".into(),
            mounts: "pgdata".into(),
            image: "postgres:16".into(),
            state: "exited".into(),
        }],
        ..Scene::default()
    };
    let rig = Rig::new(&s);
    let env = rig.env();
    let t = DockerVolumesTarget::new(None);
    let mut sel = sel(None);
    sel.volumes = vec!["pgdata".into(), "fresh".into(), "scratch".into()];
    let _ = plan_then_apply(vec![&t], &env, &sel);
    assert_eq!(
        rig.runner.mutating_calls(),
        0,
        "{:?}",
        volume_rms(&rig.runner)
    );
}

#[test]
fn safety_docker_volume_database_needs_its_exact_name() {
    let mut pg = Vol::anon(60);
    pg.files = Some(vec!["PG_VERSION".into()]);
    let s = Scene {
        volumes: vec![pg],
        ..Scene::default()
    };
    let rig = Rig::new(&s);
    let env = rig.env();
    let t = DockerVolumesTarget::new(None);
    let _ = plan_then_apply(vec![&t], &env, &sel(Some(DockerVolumes::Anonymous)));
    assert_eq!(rig.runner.mutating_calls(), 0);
}

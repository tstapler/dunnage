#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! `DockerCli` over a `FakeCommandRunner` with the Docker 29.8.2 fixtures (Story 4.1.1; validation T-180 to
//! T-182, T-399 to T-401). Nothing here touches a real daemon.

#[path = "common/docker.rs"]
mod docker_common;
use docker_common::*;

use disk_cleanup_adapters::docker_cli::{
    parse_images, parse_ps, parse_system_df, parse_volume_inspect, parse_volume_ls, DockerCli,
};
use disk_cleanup_core::docker::{DockerClient, DockerError};
use disk_cleanup_core::target::KindGate;
use disk_cleanup_core::testing::FakeCommandRunner;
use disk_cleanup_core::{DeleteError, SkipReason};
use serde_json::Value;

fn mutants(prefix: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(fixture_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with(&format!("{prefix}.mut-")))
        .collect();
    v.sort();
    v
}

#[test]
fn client_should_list_the_whole_inventory_When_fixtures_from_docker_29_8_2() {
    let r = healthy();
    let cli = DockerCli::new(&r, None);
    let probe = cli.probe();
    assert_eq!(probe.gate, KindGate::Available);
    assert_eq!(probe.server_version.as_deref(), Some("29.8.2"));
    let inv = cli.inventory().unwrap();
    assert_eq!((inv.images.len(), inv.containers.len()), (3, 2));
    assert_eq!(inv.volumes.len(), 3);
    let anon = inv.volumes.iter().find(|v| v.anonymous).unwrap();
    assert_eq!(
        (anon.approx_size_bytes, anon.links),
        (Some(1_049_000), Some(1))
    );
    assert_eq!(inv.build_cache.len(), 2);
    let pg = cli
        .volume_inspect("dc-spike-pg")
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(pg.created_at.is_some());
}

#[test]
fn daemon_down_should_gate_all_docker_kinds_Unavailable_DaemonUnavailable_and_exit_0_in_plan_and_1_only_when_named_with_apply(
) {
    let msg = "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?";
    let r = FakeCommandRunner::new();
    r.on(CTX, ok("default\n")).on(INFO, failed(msg));
    let p = DockerCli::new(&r, None).probe();
    assert_eq!(p.gate, KindGate::Unavailable(SkipReason::DaemonUnavailable));
    assert_eq!(p.detail.as_deref(), Some(msg));
    // the exit-code half (0 in plan mode, 1 only when `docker` is named with --apply) is decided by the
    // executor from this gate (Story 1.1.4, validation T-434); this adapter only supplies the gate.

    let r = FakeCommandRunner::new();
    r.on(CTX, Err(DeleteError::Timeout));
    let p = DockerCli::new(&r, None).probe();
    assert_eq!(p.gate, KindGate::Unavailable(SkipReason::DaemonUnavailable));
    assert!(p.detail.unwrap().contains("timed out"));

    let r = FakeCommandRunner::new();
    r.absent("docker");
    let p = DockerCli::new(&r, None).probe();
    assert_eq!(p.gate, KindGate::Unavailable(SkipReason::ToolMissing));
}

#[test]
fn non_local_context_should_skip_docker_with_reason_When_tcp_endpoint() {
    let r = FakeCommandRunner::new();
    r.on(CTX, ok("remote-ci\n"));
    let p = DockerCli::new(&r, None).probe();
    assert_eq!(
        p.gate,
        KindGate::Unavailable(SkipReason::NonLocalDockerContext)
    );
    assert_eq!(
        p.detail.as_deref(),
        Some("non-local docker context 'remote-ci'")
    );
    assert_eq!(r.calls().len(), 1, "only the context probe was issued");

    let r = healthy();
    let p = DockerCli::new(&r, Some("tcp://10.0.0.9:2375".into())).probe();
    assert_eq!(
        p.gate,
        KindGate::Unavailable(SkipReason::NonLocalDockerContext)
    );
    assert_eq!(r.calls().len(), 1);
}

fn mutated_lines(fixture_name: &str) -> Vec<(String, String)> {
    let base = fixture(fixture_name);
    let first: Value = serde_json::from_str(base.lines().next().unwrap()).unwrap();
    let mut out = Vec::new();
    for key in first.as_object().unwrap().keys() {
        let mut removed = first.clone();
        removed.as_object_mut().unwrap().remove(key);
        let mut renamed = first.clone();
        let v = renamed.as_object_mut().unwrap().remove(key).unwrap();
        renamed
            .as_object_mut()
            .unwrap()
            .insert(format!("{key}X"), v);
        let mut retyped = first.clone();
        retyped
            .as_object_mut()
            .unwrap()
            .insert(key.clone(), Value::from(12345));
        out.push((format!("removed-{key}"), removed.to_string()));
        out.push((format!("renamed-{key}"), renamed.to_string()));
        out.push((format!("retyped-{key}"), retyped.to_string()));
    }
    let line = base.lines().next().unwrap();
    out.push(("truncated".into(), line[..line.len() / 2].to_string()));
    out
}

/// Fields the parser does not need (and so does not require) are exempt from the removed/renamed cases.
fn required(fixture_name: &str, what: &str) -> bool {
    const IMAGES: &[&str] = &[
        "ID",
        "Repository",
        "Tag",
        "Digest",
        "Size",
        "CreatedAt",
        "Containers",
    ];
    const PS: &[&str] = &["ID", "Names", "Mounts", "State"];
    let field = what.split_once('-').map_or("", |(_, f)| f);
    if what == "truncated" {
        return true;
    }
    match fixture_name {
        "docker_images.jsonl" => IMAGES.contains(&field),
        _ => PS.contains(&field),
    }
}

#[test]
fn mutated_fixtures_should_make_the_kind_Unavailable_UnrecognisedOutput_and_never_a_partial_list_When_field_removed_renamed_retyped_emptied_or_truncated(
) {
    // file mutants of the three recorded shapes
    for n in mutants("volume_ls") {
        let raw = fixture(&n);
        if n.ends_with("mut-empty.jsonl") {
            // an empty `volume ls` is only believable when `system df -v` lists no volume either
            let r = healthy_but(&[VOLS]);
            r.on(VOLS, ok(&raw));
            let e = DockerCli::new(&r, None).volumes().unwrap_err();
            assert_eq!(
                e.gate(),
                KindGate::Unavailable(SkipReason::UnrecognisedOutput),
                "{n}"
            );
            continue;
        }
        let e = parse_volume_ls(&raw).unwrap_err();
        assert!(matches!(e, DockerError::Unrecognised(_)), "{n}");
    }
    for n in mutants("system_df_v") {
        let e = parse_system_df(&fixture(&n)).unwrap_err();
        assert_eq!(
            e.gate(),
            KindGate::Unavailable(SkipReason::UnrecognisedOutput),
            "{n}"
        );
    }
    for n in mutants("volume_inspect") {
        let e = parse_volume_inspect(&fixture(&n)).unwrap_err();
        assert!(matches!(e, DockerError::Unrecognised(_)), "{n}");
    }
    // generated mutants of images and ps (the spike captured no files for them)
    for name in ["docker_images.jsonl", "ps_a.jsonl"] {
        for (what, line) in mutated_lines(name) {
            if !required(name, &what) {
                continue;
            }
            let r = if name == "docker_images.jsonl" {
                parse_images(&line).map(|v| v.len())
            } else {
                parse_ps(&line).map(|v| v.len())
            };
            assert!(r.is_err(), "{name} {what} parsed: {line}");
        }
    }
    // emptied listings fail closed against the df counts
    let r = healthy_but(&[IMAGES]);
    r.on(IMAGES, ok(""));
    assert!(DockerCli::new(&r, None).images().is_err());
    let r = healthy_but(&[VOLS]);
    r.on(VOLS, ok(""));
    assert!(DockerCli::new(&r, None).inventory().is_err());
    // a mutated df stops the inventory before any list is returned
    let r = healthy_but(&[DF]);
    r.on(
        DF,
        ok(&fixture("system_df_v.mut-removed-Volumes.Size.jsonl")),
    );
    assert!(DockerCli::new(&r, None).inventory().is_err());
}

#[test]
fn unmutated_fixtures_should_parse() {
    assert!(parse_volume_ls(&fixture("volume_ls.jsonl")).is_ok());
    assert!(parse_system_df(&fixture("system_df_v.jsonl")).is_ok());
    assert!(parse_images(&fixture("docker_images.jsonl")).is_ok());
    assert!(parse_ps(&fixture("ps_a.jsonl")).is_ok());
    for line in fixture("volume_inspect.jsonl").lines() {
        assert!(parse_volume_inspect(line).is_ok());
    }
}

#[test]
fn empty_ps_a_with_containers_in_system_df_should_make_docker_Unavailable_and_no_volume_removable_When_counts_differ(
) {
    let r = healthy_but(&[PS]);
    r.on(PS, ok(""));
    let cli = DockerCli::new(&r, None);
    let e = cli.inventory().unwrap_err();
    assert_eq!(
        e.gate(),
        KindGate::Unavailable(SkipReason::UnrecognisedOutput)
    );
    assert!(
        e.to_string()
            .contains("0 containers listed but system df -v counts 2"),
        "{e}"
    );
    assert!(cli.containers().is_err());

    // one container short is as bad as none
    let one = fixture("ps_a.jsonl").lines().next().unwrap().to_string();
    let r = healthy_but(&[PS]);
    r.on(PS, ok(&one));
    assert!(DockerCli::new(&r, None).inventory().is_err());
}

#[test]
fn docker_server_version_outside_the_tested_range_should_make_the_kinds_ListOnly_DockerVersionUntested_When_probed(
) {
    for v in ["30.0.1\n", "28.5.1\n", "1.0.0\n"] {
        let r = healthy_but(&[VERSION]);
        r.on(VERSION, ok(v));
        let p = DockerCli::new(&r, None).probe();
        assert_eq!(
            p.gate,
            KindGate::ListOnly(SkipReason::DockerVersionUntested),
            "{v}"
        );
        assert!(p.detail.unwrap().contains("outside the tested range"));
    }
    let r = healthy_but(&[VERSION]);
    r.on(VERSION, ok("banana\n"));
    assert_eq!(
        DockerCli::new(&r, None).probe().gate,
        KindGate::Unavailable(SkipReason::UnrecognisedOutput)
    );
}

#[test]
fn only_read_only_allowlisted_commands_should_be_issued_When_listing() {
    use disk_cleanup_core::testing::CallKind;
    let r = healthy();
    let cli = DockerCli::new(&r, None);
    let _ = cli.probe();
    let _ = cli.inventory().unwrap();
    let _ = cli.volume_inspect("dc-spike-named");
    assert!(r.calls_of(CallKind::RunMutating).is_empty());
    assert_eq!(r.mutating_calls(), 0);
}

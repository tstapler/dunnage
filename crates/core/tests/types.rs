#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::path::PathBuf;

use disk_cleanup_core::*;

fn node_modules_candidate() -> Candidate {
    let path = PathBuf::from("/home/u/code/app/node_modules");
    Candidate::new(
        Locator::Path(path),
        KindFacts::BuildDir(BuildDirFacts {
            flavor: BuildDirFlavor::NodeModules,
            group: PathBuf::from("/home/u/code/app"),
            age_secs: 86_400,
            lockfile_hash: Some(7),
        }),
        AllocatedBytes(4096),
        Some(1_700_000_000),
        Identity { dev: 28, ino: 77 },
    )
}

fn sample_facts() -> Vec<(KindFacts, SelectorId, Kind)> {
    let bd = |flavor| {
        KindFacts::BuildDir(BuildDirFacts {
            flavor,
            group: "/g".into(),
            age_secs: 1,
            lockfile_hash: None,
        })
    };
    vec![
        (
            KindFacts::Cache(CacheFacts {
                label: "npm _cacache".into(),
                apt: false,
            }),
            SelectorId::Caches,
            Kind::Cache,
        ),
        (
            KindFacts::Cache(CacheFacts {
                label: "apt".into(),
                apt: true,
            }),
            SelectorId::CacheApt,
            Kind::Cache,
        ),
        (
            KindFacts::Worktree(WorktreeFacts {
                main_repo: "/r".into(),
                branch: None,
            }),
            SelectorId::Worktrees,
            Kind::Worktree,
        ),
        (
            bd(BuildDirFlavor::NodeModules),
            SelectorId::BuildDirs,
            Kind::NodeModules,
        ),
        (
            bd(BuildDirFlavor::CargoTarget),
            SelectorId::BuildDirs,
            Kind::CargoTarget,
        ),
        (
            KindFacts::Docker(DockerFacts::DanglingImage {
                image_id: "sha256:a".into(),
            }),
            SelectorId::Docker,
            Kind::DockerImage,
        ),
        (
            KindFacts::Docker(DockerFacts::BuildCache),
            SelectorId::Docker,
            Kind::DockerBuildCache,
        ),
        (
            KindFacts::Docker(DockerFacts::UnusedImage {
                image_id: "sha256:b".into(),
                tags: vec!["x:1".into()],
            }),
            SelectorId::DockerImages,
            Kind::DockerImage,
        ),
        (
            KindFacts::Docker(DockerFacts::Volume {
                name: "v".into(),
                anonymous: false,
            }),
            SelectorId::DockerVolumes,
            Kind::DockerVolume,
        ),
        (
            KindFacts::Steam(SteamFacts::Prefix { app_id: AppId(1) }),
            SelectorId::Steam,
            Kind::SteamPrefix,
        ),
        (
            KindFacts::Steam(SteamFacts::ShaderCache { app_id: AppId(1) }),
            SelectorId::Steam,
            Kind::SteamShaderCache,
        ),
        (
            KindFacts::Steam(SteamFacts::Remnant {
                install_dir: "Game".into(),
            }),
            SelectorId::Steam,
            Kind::SteamRemnant,
        ),
        (
            KindFacts::Btrfs(BtrfsFacts::Balance { usage_pct: 5 }),
            SelectorId::Btrfs,
            Kind::BtrfsBalance,
        ),
        (
            KindFacts::Btrfs(BtrfsFacts::Trim),
            SelectorId::BtrfsTrim,
            Kind::BtrfsTrim,
        ),
    ]
}

#[test]
fn candidate_should_serialize_kind_node_modules_schema_1_and_roundtrip_equal_When_serde_json() {
    let c = node_modules_candidate();
    let v: serde_json::Value = serde_json::to_value(&c).unwrap();
    assert_eq!(v["kind"], "node-modules");
    assert_eq!(v["selector"], "build-dirs");
    assert_eq!(v["schema"], 1);
    assert_eq!(v["est_bytes"], 4096);
    let text = serde_json::to_string(&c).unwrap();
    let back: Candidate = serde_json::from_str(&text).unwrap();
    assert_eq!(back, c);
}

#[test]
fn candidate_should_refuse_a_wire_form_whose_selector_or_kind_disagrees_with_its_facts() {
    let c = node_modules_candidate();
    let mut v = serde_json::to_value(&c).unwrap();
    v["selector"] = "steam".into();
    assert!(serde_json::from_value::<Candidate>(v).is_err());
    let mut v = serde_json::to_value(&c).unwrap();
    v["kind"] = "cache".into();
    assert!(serde_json::from_value::<Candidate>(v).is_err());
    let mut v = serde_json::to_value(&c).unwrap();
    v["schema"] = 2.into();
    assert!(serde_json::from_value::<Candidate>(v).is_err());
}

#[test]
fn candidate_id_should_be_stable_16_hex_and_depend_on_kind_and_locator() {
    let c = node_modules_candidate();
    let again = node_modules_candidate();
    assert_eq!(c.id(), again.id());
    assert_eq!(c.id().to_string().len(), 16);
    // Pinned (FNV-1a of "node-modules\0path\0<path>") so the audit log and saved plans keep their ids.
    assert_eq!(c.id().to_string(), "c8e494e213b90d41");
    let other = CandidateId::new(
        Kind::CargoTarget,
        &Locator::Path("/home/u/code/app/node_modules".into()),
    );
    assert_ne!(other, c.id());
    let docker = CandidateId::new(Kind::DockerVolume, &Locator::Docker(DockerId("v".into())));
    assert_ne!(docker, other);
    assert_eq!(CandidateId::from_hex(&c.id().to_string()), Some(c.id()));
    assert_eq!(CandidateId::from_hex("XYZ"), None);
}

#[test]
fn selector_of_should_map_every_facts_variant_to_its_kinds_id_and_only_build_dirs_docker_volumes_steam_should_be_destructive_When_checked(
) {
    for (facts, selector, kind) in sample_facts() {
        assert_eq!(selector_of(&facts), selector, "{facts:?}");
        assert_eq!(facts.kind(), kind, "{facts:?}");
        let c = Candidate::new(
            Locator::Path("/p".into()),
            facts,
            AllocatedBytes(0),
            None,
            Identity { dev: 1, ino: 1 },
        );
        assert_eq!(c.selector(), selector);
        assert_eq!(c.kind(), kind);
        assert!(
            selector.kinds().contains(&kind),
            "{selector:?} lists {kind:?}"
        );
    }
    let destructive: Vec<_> = SelectorId::ALL
        .iter()
        .filter(|s| s.is_destructive())
        .map(|s| s.id())
        .collect();
    assert_eq!(destructive, ["build-dirs", "docker-volumes", "steam"]);
    let tiers: Vec<_> = SelectorId::ALL.iter().map(|s| (s.id(), s.tier())).collect();
    assert_eq!(
        tiers,
        [
            ("caches", Risk::Safe),
            ("cache-apt", Risk::Safe),
            ("worktrees", Risk::Guarded),
            ("build-dirs", Risk::Destructive),
            ("docker", Risk::Guarded),
            ("docker-images", Risk::Guarded),
            ("docker-volumes", Risk::Destructive),
            ("steam", Risk::Destructive),
            ("btrfs", Risk::Guarded),
            ("btrfs-trim", Risk::Guarded),
        ]
    );
    // Steam's members: prefix and shader cache destructive, remnant guarded; the family takes the highest.
    assert_eq!(Kind::SteamRemnant.risk(), Risk::Guarded);
    assert_eq!(Kind::SteamPrefix.risk(), Risk::Destructive);
}

#[test]
fn every_selector_and_kind_should_serialize_to_its_documented_id() {
    for s in SelectorId::ALL {
        assert_eq!(serde_json::to_value(s).unwrap(), s.id());
    }
    for k in Kind::ALL {
        assert_eq!(serde_json::to_value(k).unwrap(), k.id());
    }
    assert_eq!(
        serde_json::to_value(Risk::Destructive).unwrap(),
        "destructive"
    );
    // Every selector's member kinds are all reachable from some facts, and every kind sits behind one.
    for k in Kind::ALL {
        assert!(
            SelectorId::ALL.iter().any(|s| s.kinds().contains(&k)),
            "{k:?}"
        );
    }
}

#[test]
fn outcome_should_serialize_est_bytes_and_measured_bytes_and_freed_When_serde_json() {
    let o = Outcome {
        ts: 1,
        run_id: "r".into(),
        kind: Kind::Cache,
        selector: SelectorId::Caches,
        id: node_modules_candidate().id(),
        path: Some("/p".into()),
        status: Status::Skipped(SkipReason::KindNotSelected),
        est_bytes: 10,
        measured_bytes: Some(8),
        freed: Freed {
            attributed_bytes: AllocatedBytes(8),
            apparent_bytes: ApparentBytes(9),
            entries_removed: 2,
            inodes_freed: 1,
        },
        elapsed_ms: 3,
    };
    let v = serde_json::to_value(&o).unwrap();
    assert_eq!(v["est_bytes"], 10);
    assert_eq!(v["measured_bytes"], 8);
    assert_eq!(v["freed"]["attributed_bytes"], 8);
    assert_eq!(v["status"]["skipped"], "kind-not-selected");
    let back: Outcome = serde_json::from_value(v).unwrap();
    assert_eq!(back, o);
}

#[test]
fn app_id_at_or_above_2_pow_31_should_be_a_non_steam_shortcut() {
    assert!(!AppId((1 << 31) - 1).is_non_steam_shortcut());
    assert!(AppId(1 << 31).is_non_steam_shortcut());
}

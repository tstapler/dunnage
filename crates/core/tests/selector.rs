#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names,
    non_snake_case
)]
//! `KindSelector` and `Selection` (Task 1.1.4c).

mod common;

use common::{build_dir, docker_cache, run, FakeTarget, Rig, DAY};
use dunnage_core::plan::Planned;
use dunnage_core::selector::{
    default_min_age_secs, Discard, DockerVolumes, KindSelector, Selection, SelectorError,
};
use dunnage_core::{SelectorId, SkipReason};

const ALL_BUILT: [SelectorId; 11] = SelectorId::ALL;

fn parse(s: &str) -> Result<KindSelector, SelectorError> {
    KindSelector::parse(s, &ALL_BUILT)
}

fn ids(s: &KindSelector) -> Vec<&'static str> {
    let mut v: Vec<_> = s.ids().map(SelectorId::id).collect();
    v.sort_unstable();
    v
}

#[test]
fn kind_selector_should_expand_families_replace_not_add_and_reject_unknown_all_plus_and_unbuilt_ids_with_did_you_mean_or_availability_message_When_parsed(
) {
    // did-you-mean
    let e = parse("dockr").unwrap_err();
    assert_eq!(
        e.to_string(),
        "unknown kind 'dockr'. Did you mean 'docker'? See 'dunnage kinds'."
    );
    // nothing within edit distance 2: no suggestion
    assert_eq!(
        parse("frobnicate").unwrap_err().to_string(),
        "unknown kind 'frobnicate'. See 'dunnage kinds'."
    );
    for bad in ["all", "+build-dirs", ""] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
    assert!(matches!(parse("").unwrap_err(), SelectorError::Empty));
    // a family id is the selector of that name: docker is the guarded one alone, btrfs has no trim
    assert_eq!(ids(&parse("docker").unwrap()), ["docker"]);
    assert_eq!(ids(&parse("btrfs").unwrap()), ["btrfs"]);
    assert_eq!(
        ids(&parse("btrfs,btrfs-trim").unwrap()),
        ["btrfs", "btrfs-trim"]
    );
    // --kinds X replaces the default set
    let only = parse("caches").unwrap();
    assert!(!only.contains(SelectorId::Worktrees));
    assert_eq!(ids(&KindSelector::default_apply()), ["caches", "worktrees"]);
    // an id whose kind is not registered in this build
    let built = [SelectorId::Caches, SelectorId::Worktrees];
    assert_eq!(
        KindSelector::parse("steam", &built)
            .unwrap_err()
            .to_string(),
        "error: 'steam' is not available in this build (see 'dunnage kinds')"
    );
}

#[test]
fn except_and_min_size_and_per_kind_min_age_defaults_should_apply_When_14d_14d_30d_30d_none() {
    let d = |id| default_min_age_secs(id).map(|s| s / DAY as u64);
    assert_eq!(d(SelectorId::Worktrees), Some(14));
    assert_eq!(d(SelectorId::BuildDirs), Some(14));
    assert_eq!(d(SelectorId::DockerImages), Some(30));
    assert_eq!(d(SelectorId::DockerVolumes), Some(30));
    assert_eq!(d(SelectorId::Steam), Some(30));
    for none in [
        SelectorId::Caches,
        SelectorId::CacheApt,
        SelectorId::Docker,
        SelectorId::Btrfs,
        SelectorId::BtrfsTrim,
    ] {
        assert_eq!(d(none), None, "{none:?}");
    }

    // --min-age 7d overrides all, but a build dir keeps the 60 minute floor and a worktree 24 h
    let mut sel = Selection::new(
        parse("build-dirs,worktrees,steam").unwrap(),
        KindSelector::default(),
    );
    sel.min_age_secs = Some(7 * DAY as u64);
    assert_eq!(
        sel.effective_min_age(SelectorId::Steam),
        Some(7 * DAY as u64)
    );
    sel.min_age_secs = Some(60);
    assert_eq!(sel.effective_min_age(SelectorId::BuildDirs), Some(3600));
    assert_eq!(
        sel.effective_min_age(SelectorId::Worktrees),
        Some(DAY as u64)
    );
    sel.min_age_secs = Some(0);
    assert_eq!(sel.effective_min_age(SelectorId::BuildDirs), Some(3600));
    assert_eq!(sel.effective_min_age(SelectorId::Caches), Some(0));

    // the hidden --stale-days 7 equals --min-age 7d
    let mut a = Selection::new(parse("build-dirs").unwrap(), KindSelector::default());
    a.stale_days(7);
    let mut b = a.clone();
    b.min_age_secs = Some(7 * DAY as u64);
    assert_eq!(a, b);
    assert_eq!(a.flags(), "--min-age 7d");

    // --except docker-images drops the kind; --min-size 1GiB drops small items; defaults decide age
    let rig = Rig::new();
    let mut except = Selection::new(
        parse("docker,docker-images,build-dirs").unwrap(),
        parse("docker-images").unwrap(),
    );
    except.min_size_bytes = Some(1 << 30);
    let big_old = build_dir(1, 2 << 30, 20);
    let small_old = build_dir(2, 1 << 20, 20);
    let small_old_id = small_old.id();
    let big_young = build_dir(3, 2 << 30, 5);
    let t = FakeTarget::new(
        &[SelectorId::BuildDirs],
        vec![big_old.clone(), small_old, big_young.clone()],
    );
    let mut d = FakeTarget::new(
        &[SelectorId::Docker, SelectorId::DockerImages],
        vec![docker_cache(1, 3 << 30)],
    );
    d.name = "docker";
    let r = run(&rig, &[&t, &d], &except, false, 1, None).unwrap();
    let by_id = |id| {
        r.plan
            .items
            .iter()
            .find(|i| i.candidate.id() == id)
            .map(|i| i.planned)
    };
    assert!(by_id(small_old_id).is_none(), "under --min-size: absent");
    assert_eq!(by_id(big_old.id()), Some(Planned::Will));
    assert_eq!(
        by_id(big_young.id()),
        Some(Planned::WillNot(SkipReason::TooRecent))
    );
}

#[test]
fn a_kind_specific_flag_should_be_a_usage_error_when_its_kind_is_not_selected() {
    let mut sel = Selection::new(parse("docker").unwrap(), KindSelector::default());
    sel.docker_volumes = Some(DockerVolumes::Anonymous);
    assert_eq!(
        sel.validate().unwrap_err().to_string(),
        "error: --docker-volumes only applies to docker-volumes, which is not in --kinds (docker). Add docker-volumes to --kinds or drop the flag. Nothing was deleted."
    );
    let mut sel = Selection::new(parse("caches").unwrap(), KindSelector::default());
    sel.steam_saves = Some(Discard::Discard);
    assert!(sel.validate().is_err());
    sel.steam_saves = None;
    sel.keep_newest = Some(1);
    assert!(sel.validate().is_err());
    let mut ok = Selection::new(parse("build-dirs,steam").unwrap(), KindSelector::default());
    ok.keep_newest = Some(1);
    ok.steam_saves = Some(Discard::Discard);
    assert!(ok.validate().is_ok());
}

#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! The pure volume decision (Story 4.1.4; validation T-186, T-402, T-403, T-405) and the image decisions of
//! Stories 4.1.2 and 4.1.3.

use dunnage_core::selector::DockerVolumes;
use dunnage_core::targets::docker_image_guard::{
    decide_dangling_image, decide_unused_image, ImageFacts,
};
use dunnage_core::targets::docker_volume_guard::{
    database_signature, decide_volume, evidence_text, Contents, VolumeFacts, VolumeOpts,
    COMPOSE_DETAIL,
};
use dunnage_core::SkipReason;

const NOW: i64 = 1_800_000_000;
const DAY: i64 = 86_400;

fn vol(name: &str, anonymous: bool) -> VolumeFacts {
    VolumeFacts {
        name: name.into(),
        anonymous,
        used_by: None,
        compose_project_present: false,
        approx_size_bytes: Some(1_049_000),
        created_epoch: Some(NOW - 40 * DAY),
        contents: Contents::Names(vec!["data.txt".into()]),
    }
}

fn opts(class: Option<DockerVolumes>, named: &[String]) -> VolumeOpts<'_> {
    VolumeOpts {
        class,
        named,
        min_age_secs: 30 * 86_400,
        now: NOW,
    }
}

fn reason(f: &VolumeFacts, o: &VolumeOpts<'_>) -> Option<SkipReason> {
    decide_volume(f, o).reason
}

#[test]
fn volume_referenced_by_stopped_container_or_compose_project_should_be_Skipped_VolumeReferenced_When_decided(
) {
    let mut f = vol("pgdata", false);
    f.used_by = Some("db".into());
    let o = opts(Some(DockerVolumes::Named), &[]);
    let v = decide_volume(&f, &o);
    assert_eq!(v.reason, Some(SkipReason::VolumeReferenced));
    assert_eq!(v.detail.as_deref(), Some("db"));

    let mut f = vol("proj_cache", false);
    f.compose_project_present = true;
    let v = decide_volume(&f, &o);
    assert_eq!(v.reason, Some(SkipReason::VolumeReferenced));
    assert_eq!(v.detail.as_deref(), Some(COMPOSE_DETAIL));

    // --volume never lifts a reference
    let named = vec!["pgdata".to_string()];
    let mut f = vol("pgdata", false);
    f.used_by = Some("db".into());
    assert_eq!(
        reason(&f, &opts(Some(DockerVolumes::Named), &named)),
        Some(SkipReason::VolumeReferenced)
    );
}

#[test]
fn unreferenced_old_small_volume_should_be_OptInRequired_until_its_own_flag_is_given_When_decided()
{
    let anon = vol(&"a".repeat(64), true);
    let named = vol("scratch", false);
    let none = opts(None, &[]);
    assert_eq!(reason(&anon, &none), Some(SkipReason::OptInRequired));
    assert_eq!(reason(&named, &none), Some(SkipReason::OptInRequired));
    let a = opts(Some(DockerVolumes::Anonymous), &[]);
    assert_eq!(reason(&anon, &a), None);
    assert_eq!(reason(&named, &a), Some(SkipReason::OptInRequired));
    let n = opts(Some(DockerVolumes::Named), &[]);
    assert_eq!(reason(&named, &n), None);
    assert_eq!(reason(&anon, &n), Some(SkipReason::OptInRequired));
}

#[test]
fn anonymous_volume_created_3_days_ago_should_be_TooRecent_with_the_default_30d_floor_independent_of_image_min_age_When_decided(
) {
    let mut f = vol(&"b".repeat(64), true);
    f.created_epoch = Some(NOW - 3 * DAY);
    let o = opts(Some(DockerVolumes::Anonymous), &[]);
    assert_eq!(reason(&f, &o), Some(SkipReason::TooRecent));
    // an unreadable creation time fails closed
    f.created_epoch = None;
    assert_eq!(reason(&f, &o), Some(SkipReason::TooRecent));
    // exactly at the floor is old enough
    f.created_epoch = Some(NOW - 30 * DAY);
    assert_eq!(reason(&f, &o), None);
    // --volume does not lift the age
    let named = vec![f.name.clone()];
    f.created_epoch = Some(NOW - 3 * DAY);
    assert_eq!(
        reason(&f, &opts(Some(DockerVolumes::Anonymous), &named)),
        Some(SkipReason::TooRecent)
    );
}

#[test]
fn volume_over_1gib_with_a_database_signature_or_with_unreadable_contents_should_be_LooksLikeDatabase_or_ContentsUnknown_and_volume_flag_should_lift_only_those_When_decided(
) {
    let o = opts(Some(DockerVolumes::Anonymous), &[]);
    for sig in [
        "PG_VERSION",
        "ibdata1",
        "dump.rdb",
        "mongod.lock",
        "app.sqlite",
    ] {
        let mut f = vol(&"c".repeat(64), true);
        f.contents = Contents::Names(vec!["x".into(), sig.into()]);
        let v = decide_volume(&f, &o);
        assert_eq!(v.reason, Some(SkipReason::LooksLikeDatabase), "{sig}");
        assert_eq!(v.detail.as_deref(), Some(sig));
    }
    let mut big = vol(&"c".repeat(64), true);
    big.approx_size_bytes = Some((1 << 30) + 1);
    assert_eq!(reason(&big, &o), Some(SkipReason::LooksLikeDatabase));
    big.approx_size_bytes = Some(1 << 30);
    assert_eq!(reason(&big, &o), None);

    let mut unreadable = vol(&"d".repeat(64), true);
    unreadable.contents = Contents::Unreadable;
    assert_eq!(reason(&unreadable, &o), Some(SkipReason::ContentsUnknown));
    let mut nosize = vol(&"e".repeat(64), true);
    nosize.approx_size_bytes = None;
    assert_eq!(reason(&nosize, &o), Some(SkipReason::ContentsUnknown));

    // --volume lifts the heuristics and the size rule
    let mut lifted = vol(&"f".repeat(64), true);
    lifted.contents = Contents::Names(vec!["PG_VERSION".into()]);
    lifted.approx_size_bytes = Some(5 << 30);
    let named = vec![lifted.name.clone()];
    let ol = opts(Some(DockerVolumes::Anonymous), &named);
    assert_eq!(reason(&lifted, &ol), None);
    lifted.contents = Contents::Unreadable;
    assert_eq!(reason(&lifted, &ol), None);
    // but not the class opt-in
    assert_eq!(
        reason(&lifted, &opts(None, &named)),
        Some(SkipReason::OptInRequired)
    );
}

#[test]
fn database_signature_should_match_only_top_level_names_When_listed() {
    assert_eq!(
        database_signature(&["PG_VERSION".into()]).as_deref(),
        Some("PG_VERSION")
    );
    assert_eq!(
        database_signature(&["x.sqlite3".into(), "rdb".into()]),
        None
    );
}

#[test]
fn evidence_should_show_size_created_at_mount_point_and_class_When_formatted() {
    let t = evidence_text(
        Some(1_049_000),
        Some("2026-11-06T13:35:26-08:00"),
        "/var/lib/docker/volumes/x/_data",
        true,
    );
    assert_eq!(
        t,
        "~1.049MB, created 2026-11-06T13:35:26-08:00, /var/lib/docker/volumes/x/_data, anonymous"
    );
    assert!(
        evidence_text(None, None, "/m", false).contains("size unknown, created unknown, /m, named")
    );
}

fn img(containers: Option<u32>, digest: Option<&str>, age_days: i64) -> ImageFacts {
    ImageFacts {
        containers,
        named_by_container: false,
        digest: digest.map(Into::into),
        created_epoch: Some(NOW - age_days * DAY),
    }
}

#[test]
fn unused_image_should_be_decided_referenced_then_local_then_age_When_decided() {
    let sha = Some("sha256:ab");
    let min = 30 * 86_400;
    assert_eq!(decide_unused_image(&img(Some(0), sha, 60), min, NOW), None);
    assert_eq!(
        decide_unused_image(&img(Some(0), sha, 3), min, NOW),
        Some(SkipReason::TooRecent)
    );
    assert_eq!(
        decide_unused_image(&img(Some(1), sha, 60), min, NOW),
        Some(SkipReason::ImageReferenced)
    );
    assert_eq!(
        decide_unused_image(&img(Some(0), None, 60), min, NOW),
        Some(SkipReason::LocalOnlyImage)
    );
    // an unknown container count is a reference
    assert_eq!(
        decide_unused_image(&img(None, sha, 60), min, NOW),
        Some(SkipReason::ImageReferenced)
    );
    let mut named = img(Some(0), sha, 60);
    named.named_by_container = true;
    assert_eq!(
        decide_unused_image(&named, min, NOW),
        Some(SkipReason::ImageReferenced)
    );
    let mut nodate = img(Some(0), sha, 60);
    nodate.created_epoch = None;
    assert_eq!(
        decide_unused_image(&nodate, min, NOW),
        Some(SkipReason::TooRecent)
    );
}

#[test]
fn dangling_image_should_be_stopped_only_by_a_reference_When_decided() {
    assert_eq!(decide_dangling_image(&img(Some(0), None, 0)), None);
    assert_eq!(
        decide_dangling_image(&img(Some(2), None, 0)),
        Some(SkipReason::ImageReferenced)
    );
    assert_eq!(
        decide_dangling_image(&img(None, None, 0)),
        Some(SkipReason::ImageReferenced)
    );
}

#[test]
fn docker_refusals_should_map_by_message_to_skips_and_other_failures_stay_errors() {
    use dunnage_core::DeleteError;
    let f = |m: &str| DeleteError::CommandFailed {
        status: 1,
        message: m.into(),
    };
    assert_eq!(
        f("Error response from daemon: remove x: volume is in use - [abc]").skip_reason(),
        Some(SkipReason::VolumeReferenced)
    );
    assert_eq!(
        f("Error response from daemon: conflict: unable to delete abc (must be forced) - image is being used by stopped container 1f")
            .skip_reason(),
        Some(SkipReason::ImageReferenced)
    );
    assert_eq!(f("Cannot connect to the Docker daemon").skip_reason(), None);
    assert_eq!(DeleteError::Timeout.skip_reason(), None);
}

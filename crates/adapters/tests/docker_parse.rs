#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Wire-shape parsing of the Docker 29.8.2 fixtures from spike 0.2.2 (Story 4.1.1; validation T-521,
//! T-522, T-180).

#[path = "common/docker.rs"]
mod docker_common;
use docker_common::*;

use dunnage_adapters::docker_cli::{
    parse_images, parse_ps, parse_system_df, parse_volume_inspect, parse_volume_ls,
};

#[test]
fn volume_size_should_come_only_from_system_df_v_as_an_si_string_and_inspect_should_have_no_usage_data_When_29_8_2_fixtures(
) {
    let raw_inspect = fixture("volume_inspect.jsonl");
    assert!(
        !raw_inspect.contains("UsageData"),
        "the recorded inspect has no UsageData"
    );
    let first = raw_inspect.lines().next().unwrap();
    let v = parse_volume_inspect(first).unwrap();
    assert_eq!(v.approx_size_bytes, None, "inspect carries no size");
    assert!(v.created_at.is_some());

    let with_usage = first.replace(
        "\"Driver\"",
        "\"UsageData\":{\"Size\":99999,\"RefCount\":1},\"Driver\"",
    );
    assert_eq!(
        parse_volume_inspect(&with_usage).unwrap().approx_size_bytes,
        None
    );

    let df = parse_system_df(&fixture("system_df_v.jsonl")).unwrap();
    let anon = df
        .volumes
        .iter()
        .find(|v| v.name.starts_with("6172cc6c"))
        .unwrap();
    assert_eq!(
        anon.approx_size_bytes,
        Some(1_049_000),
        "1.049MB is SI and approximate"
    );
    let named = df
        .volumes
        .iter()
        .find(|v| v.name == "dc-spike-named")
        .unwrap();
    assert_eq!(named.approx_size_bytes, Some(0));
    assert_eq!(anon.links, Some(1));

    let two_lines = format!("{0}\n{0}\n", fixture("system_df_v.jsonl").trim());
    assert!(
        parse_system_df(&two_lines).is_err(),
        "system df -v is one object, not JSONL"
    );
}

#[test]
fn labels_should_parse_as_a_string_in_ls_and_df_and_as_an_object_or_null_in_inspect() {
    let ls = parse_volume_ls(&fixture("volume_ls.jsonl")).unwrap();
    assert_eq!(ls.len(), 3);
    assert_eq!(ls.iter().filter(|v| v.anonymous).count(), 1);
    assert!(ls
        .iter()
        .find(|v| v.anonymous)
        .unwrap()
        .name
        .starts_with("6172cc6c"));

    let df = parse_system_df(&fixture("system_df_v.jsonl")).unwrap();
    let anon: Vec<_> = df
        .volumes
        .iter()
        .filter(|v| v.labels.contains_key("com.docker.volume.anonymous"))
        .collect();
    assert!(!anon.is_empty());

    let raw = fixture("volume_inspect.jsonl");
    let mut anon_seen = 0;
    let mut null_seen = 0;
    for line in raw.lines() {
        let v = parse_volume_inspect(line).unwrap();
        if line.contains("\"Labels\":null") {
            null_seen += 1;
            assert!(v.labels.is_empty() && !v.anonymous);
        }
        if v.anonymous {
            anon_seen += 1;
        }
    }
    assert_eq!((anon_seen, null_seen), (1, 2));
}

#[test]
fn docker_client_should_parse_recorded_jsonl_and_ignore_unknown_fields_When_fixtures_from_docker_29_8_2(
) {
    let images = parse_images(&fixture("docker_images.jsonl")).unwrap();
    assert_eq!(images.len(), 3);
    assert_eq!(images[0].id, "c9418950f8fb");
    assert_eq!(images[0].size_bytes, 149_000_000);
    assert_eq!(
        images[0].digest, None,
        "a locally built image prints <none>"
    );
    assert_eq!(images[0].containers, Some(0));
    assert!(!images[0].is_dangling());

    let ps = parse_ps(&fixture("ps_a.jsonl")).unwrap();
    assert_eq!(ps.len(), 2);
    assert_eq!(ps[0].names, "dc-spike-pg-ctr");

    let extra = fixture("volume_ls.jsonl").replace("\"Driver\"", "\"Surprise\":[1,2],\"Driver\"");
    assert_eq!(parse_volume_ls(&extra).unwrap().len(), 3);
    let extra = fixture("ps_a.jsonl").replace("\"ID\"", "\"Surprise\":{},\"ID\"");
    assert_eq!(parse_ps(&extra).unwrap().len(), 2);
    let extra = fixture("system_df_v.jsonl").replacen('{', "{\"Surprise\":1,", 1);
    assert!(parse_system_df(&extra).is_ok());

    let df = parse_system_df(&fixture("system_df_v.jsonl")).unwrap();
    assert_eq!(df.container_count, 2);
    assert_eq!(df.build_cache.len(), 2);
}

#[test]
fn a_dangling_image_should_have_no_repository_and_no_tag() {
    let line = r#"{"Containers":"N/A","CreatedAt":"2026-11-01 18:36:31 -0800 PST","CreatedSince":"x","Digest":"<none>","ID":"abc123def456","Repository":"<none>","SharedSize":"N/A","Size":"1.5kB","Tag":"<none>","UniqueSize":"N/A"}"#;
    let i = &parse_images(line).unwrap()[0];
    assert!(i.is_dangling());
    assert_eq!((i.size_bytes, i.containers), (1_500, None));
}

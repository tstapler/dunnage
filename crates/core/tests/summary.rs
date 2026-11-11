#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.2.2: `RunSummary::finish`, the attribution math (validation T-020..T-031, T-266, T-425, T-426,
//! T-467, T-524, T-538..T-540).

use std::path::{Path, PathBuf};

use disk_cleanup_core::candidate::Locator;
use disk_cleanup_core::ids::{AllocatedBytes, CandidateId, DockerId};
use disk_cleanup_core::ports::{ExtentAttribution, ItemExtents, SharedExtent, Unavailable};
use disk_cleanup_core::testing::FakeSpaceMeter;
use disk_cleanup_core::{
    AllocSnapshot, CauseKind, DiskVerdict, FinishInput, Freed, FsSnapshot, Held, HeldItem, Kind,
    Outcome, PreDeleteSample, RunSummary, SelectorId, Settled, SkipReason, SnapshotFacts, SpaceUse,
    StageTimings, Status, WalkerCounters,
};

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

fn gib_f(x: f64) -> u64 {
    (x * GIB as f64).round() as u64
}

fn btrfs_snap(data_used: u64) -> FsSnapshot {
    let mut s = FakeSpaceMeter::plain_snapshot("/", 0, 0);
    s.fs_type = "btrfs".into();
    let space = SpaceUse {
        bytes_used: data_used,
        total_bytes: data_used * 2,
    };
    s.btrfs = Some(AllocSnapshot {
        data: space,
        metadata: space,
        system: space,
        unallocated: 50 * GIB,
    });
    s
}

fn removed(kind: Kind, path: &str, bytes: u64) -> Outcome {
    let locator = if matches!(
        kind,
        Kind::DockerImage | Kind::DockerBuildCache | Kind::DockerVolume
    ) {
        Locator::Docker(DockerId(path.to_string()))
    } else {
        Locator::Path(PathBuf::from(path))
    };
    Outcome {
        ts: 0,
        run_id: "r".into(),
        kind,
        selector: SelectorId::Caches,
        id: CandidateId::new(kind, &locator),
        path: matches!(locator, Locator::Path(_)).then(|| PathBuf::from(path)),
        status: Status::Removed,
        est_bytes: bytes,
        measured_bytes: Some(bytes),
        freed: Freed {
            attributed_bytes: AllocatedBytes(bytes),
            ..Freed::default()
        },
        elapsed_ms: 1,
        trimmed_bytes: None,
    }
}

fn unselected(path: &str, est: u64) -> Outcome {
    let mut o = removed(Kind::CargoTarget, path, 0);
    o.status = Status::Skipped(SkipReason::KindNotSelected);
    o.est_bytes = est;
    o.measured_bytes = None;
    o
}

fn input(before: u64, after: u64, outcomes: Vec<Outcome>) -> FinishInput {
    FinishInput {
        fs_before: btrfs_snap(before),
        fs_after: Some(btrfs_snap(after)),
        settled: Some(Settled {
            stable: true,
            data_bytes_used: Some(after),
            samples: 2,
            waited_ms: 500,
        }),
        outcomes,
        extents: ExtentAttribution::Measured(Vec::new()),
        pre_delete: Vec::new(),
        delete_window_ms: 0,
        held: Vec::new(),
        snapshots: None,
        docker_delta_bytes: None,
        verdict_after: DiskVerdict::Ok,
        timings: StageTimings::default(),
        walker: WalkerCounters::default(),
        notes: Vec::new(),
    }
}

fn near(a: f64, b: f64, eps: f64) -> bool {
    (a - b).abs() <= eps
}

fn three_removals() -> Vec<Outcome> {
    vec![
        removed(Kind::CargoTarget, "/a", 40 * GIB),
        removed(Kind::NodeModules, "/b", 30 * GIB),
        removed(Kind::Cache, "/c", 26 * GIB),
    ]
}

#[test]
fn finish_should_compute_remainder_and_pct_When_96gib_attributed_of_99_7_drop() {
    let s = RunSummary::finish(input(gib_f(1730.0), gib_f(1630.3), three_removals()));
    assert_eq!(s.attributed_bytes, 96 * GIB);
    let drop = s.measured_drop.unwrap();
    assert!((drop - gib_f(99.7) as i64).abs() < 1024, "drop {drop}");
    assert_eq!(s.unattributed_text.as_deref(), Some("+3.7 GiB"));
    assert!(near(s.attribution_pct.unwrap(), 96.29, 0.01));
    assert_eq!(s.attribution_line, "Attribution 96.3% (target >= 95%)");
    assert!(!s.provisional);
}

#[test]
fn finish_should_report_negative_remainder_When_attributed_exceeds_drop() {
    let outcomes = vec![removed(Kind::CargoTarget, "/a", 110 * GIB)];
    let s = RunSummary::finish(input(1000 * GIB, 900 * GIB, outcomes));
    assert_eq!(s.unattributed_text.as_deref(), Some("-10.0 GiB"));
    assert!(near(s.attribution_pct.unwrap(), 110.0, 1e-9), "uncapped");
    assert_eq!(
        s.attribution_line,
        "Attribution 110.0% (attributed exceeds the counter drop: concurrent writes or shared extents)"
    );
}

#[test]
fn finish_should_say_below_target_and_keep_exit_code_When_attribution_is_4_6pct() {
    let outcomes = vec![removed(Kind::CargoTarget, "/a", 11 * GIB)];
    let s = RunSummary::finish(input(1000 * GIB, 759 * GIB, outcomes));
    assert_eq!(
        s.attribution_line,
        "Attribution 4.6% (below target) - see unattributed remainder"
    );
    // no exit code lives in the summary: low attribution is only a line
    assert_eq!(s.verdict_after, DiskVerdict::Ok);
}

#[test]
fn finish_should_print_na_not_nan_When_measured_drop_is_zero_or_negative() {
    for after in [1000 * GIB, 1010 * GIB] {
        let outcomes = vec![removed(Kind::CargoTarget, "/a", 11 * GIB)];
        let s = RunSummary::finish(input(1000 * GIB, after, outcomes));
        assert_eq!(s.attribution_pct, None);
        assert!(s.attribution_line.contains("n/a"), "{}", s.attribution_line);
        for text in [&s.attribution_line, &s.measured_line] {
            assert!(!text.contains("NaN") && !text.contains("inf"), "{text}");
            assert!(!text.contains('%'), "{text}");
        }
    }
}

#[test]
fn measured_drop_should_come_from_data_bytes_used_not_unallocated_When_only_unallocated_changes() {
    let mut i = input(
        1000 * GIB,
        1000 * GIB,
        vec![removed(Kind::Cache, "/a", GIB)],
    );
    i.fs_after
        .as_mut()
        .unwrap()
        .btrfs
        .as_mut()
        .unwrap()
        .unallocated = 88 * GIB;
    let s = RunSummary::finish(i);
    assert_eq!(s.measured_drop, Some(0));
    assert!(s.attribution_line.contains("n/a"));
    assert!(!s.measured_line.contains("unallocated"));
}

#[test]
fn unsettled_counter_should_label_provisional_and_point_at_status_and_never_claim_unallocated_grew_When_samples_100_90_80(
) {
    let mut i = input(
        gib_f(1730.0),
        gib_f(1689.0),
        vec![removed(Kind::Cache, "/a", 40 * GIB)],
    );
    i.settled = Some(Settled {
        stable: false,
        data_bytes_used: Some(gib_f(1689.0)),
        samples: 3,
        waited_ms: 30_000,
    });
    let s = RunSummary::finish(i);
    assert!(s.provisional);
    assert_eq!(
        s.measured_line,
        "Measured drop in data used 41.0 GiB (provisional: counter was still falling after 30s)"
    );
    assert_eq!(
        s.followup_line.as_deref(),
        Some("Run 'disk-cleanup status' in a minute for the settled figure")
    );
    assert!(s.attribution_line.contains("provisional"));
    for t in [&s.measured_line, &s.attribution_line] {
        assert!(!t.contains("unallocated"));
    }
}

#[test]
fn summary_should_carry_discover_plan_execute_settle_ms_and_dirs_scanned_cache_hits_When_any_over_100ms(
) {
    let mut i = input(
        100 * GIB,
        90 * GIB,
        vec![removed(Kind::Cache, "/a", 10 * GIB)],
    );
    i.timings = StageTimings {
        discover_ms: 120,
        plan_ms: 150,
        execute_ms: 900,
        settle_ms: 400,
    };
    i.walker = WalkerCounters {
        dirs_scanned: 77,
        cache_hits: 5,
    };
    let s = RunSummary::finish(i);
    assert_eq!(s.timings.settle_ms, 400);
    assert_eq!(s.walker.cache_hits, 5);
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["timings"]["discover_ms"], 120);
    assert_eq!(v["walker"]["dirs_scanned"], 77);
}

#[test]
fn docker_delta_should_stay_out_of_the_numerator_and_out_of_the_denominator_When_a_run_includes_docker(
) {
    let outcomes = vec![
        removed(Kind::CargoTarget, "/a", 60 * GIB),
        removed(Kind::DockerImage, "sha256:aa", 38 * GIB),
    ];
    let mut i = input(1000 * GIB, 900 * GIB, outcomes.clone());
    i.docker_delta_bytes = Some(38 * GIB);
    let s = RunSummary::finish(i);
    assert_eq!(
        s.attributed_bytes,
        60 * GIB,
        "docker bytes are not attributed"
    );
    assert!(near(s.attribution_pct.unwrap(), 96.77, 0.01));
    assert_eq!(
        s.attribution_line,
        "Attribution 96.8% over filesystem kinds (target >= 95%; docker excluded)"
    );
    assert_eq!(
        s.docker_line.as_deref(),
        Some("docker: 38.0 GiB (daemon-side; not in btrfs terms)")
    );

    let s = RunSummary::finish(input(1000 * GIB, 900 * GIB, outcomes.clone()));
    assert!(near(s.attribution_pct.unwrap(), 60.0, 1e-9));
    assert_eq!(
        s.attribution_line,
        "Attribution 60.0% (below target; docker figure unavailable, so its bytes stay in the remainder)"
    );

    let mut i = input(1000 * GIB, 950 * GIB, outcomes);
    i.docker_delta_bytes = Some(500 * GIB);
    let s = RunSummary::finish(i);
    assert_eq!(s.docker_delta_bytes, 50 * GIB, "clamped to the drop");
}

#[test]
fn snapshots_should_be_named_When_drop_is_under_10pct_of_attributed_and_no_wider_kinds_suggested() {
    let facts = SnapshotFacts {
        tool: "snapper".into(),
        entries: vec!["#12 2026-09-01".into(), "#13 2026-10-01".into()],
        list_command: "snapper list".into(),
    };
    let outcomes = vec![removed(Kind::CargoTarget, "/a", 12 * GIB)];
    let mut i = input(1000 * GIB, gib_f(999.7), outcomes.clone());
    i.snapshots = Some(facts);
    let s = RunSummary::finish(i);
    assert_eq!(
        s.snapshot_line.as_deref(),
        Some(
            "Only 0.3 GiB of 12.0 GiB came back: 2 snapshots (snapper #12 2026-09-01, #13 2026-10-01) can still hold the deleted extents. List them with 'snapper list' and delete old ones yourself; do not widen --kinds."
        )
    );
    assert!(!s.offer_wider_kinds);

    let s = RunSummary::finish(input(1000 * GIB, gib_f(999.7), outcomes));
    assert_eq!(s.snapshot_line, None);
    assert!(s.offer_wider_kinds);
    // plan defect (Story 1.2.2): the AC says "plain below target" here, but 12 attributed against a 0.3 drop
    // is 4000% by the story's own definition, which reads "attributed exceeds the counter drop"
    assert!(s
        .attribution_line
        .contains("attributed exceeds the counter drop"));
}

fn causes_input(samples: Vec<PreDeleteSample>, held: Vec<HeldItem>) -> FinishInput {
    let outcomes = vec![
        removed(Kind::CargoTarget, "/a", 3 * GIB),
        removed(Kind::CargoTarget, "/b", GIB),
        removed(Kind::CargoTarget, "/c", 104 * GIB),
    ];
    let mut i = input(1000 * GIB, 900 * GIB, outcomes);
    i.pre_delete = samples;
    i.delete_window_ms = 40_000;
    i.held = held;
    i
}

fn hold(p: &str, held: Held) -> HeldItem {
    HeldItem {
        path: PathBuf::from(p),
        held,
    }
}

#[test]
fn explained_should_need_measured_causes_summing_to_the_shortfall_within_5pct_and_be_false_for_unmeasured_or_over_explained(
) {
    // attributed 108 GiB against a drop of 100 GiB: shortfall 8 GiB
    let samples = vec![
        PreDeleteSample {
            at_ms: 0,
            data_bytes_used: 1000 * GIB,
        },
        PreDeleteSample {
            at_ms: 6_000,
            data_bytes_used: 1000 * GIB + gib_f(0.6),
        },
    ];
    let s = RunSummary::finish(causes_input(
        samples,
        vec![hold("/a", Held::Found), hold("/b", Held::Found)],
    ));
    assert_eq!(s.unattributed_text.as_deref(), Some("-8.0 GiB"));
    let by = |k| s.causes.iter().find(|c| c.kind == k).unwrap();
    let cw = by(CauseKind::ConcurrentWriteDelta);
    assert!(
        (cw.bytes - 4 * GIB as i64).abs() < MIB as i64,
        "{}",
        cw.bytes
    );
    assert_eq!(cw.source, "two data.bytes_used samples");
    let sh = by(CauseKind::SnapshotHeld);
    assert_eq!(sh.bytes, 4 * GIB as i64);
    assert_eq!(sh.source, "snapshot path probe");
    assert!(s.explained);

    // one sample, every probe Absent or Unreadable
    let one = vec![PreDeleteSample {
        at_ms: 0,
        data_bytes_used: 1000 * GIB,
    }];
    let s = RunSummary::finish(causes_input(
        one,
        vec![hold("/a", Held::Absent), hold("/b", Held::Unreadable)],
    ));
    assert!(s
        .causes
        .iter()
        .all(|c| c.bytes == 0 && c.source == "unmeasured"));
    assert!(!s.explained);

    // two samples under 2 s apart are no measurement either
    let close = vec![
        PreDeleteSample {
            at_ms: 0,
            data_bytes_used: 1000 * GIB,
        },
        PreDeleteSample {
            at_ms: 1_999,
            data_bytes_used: 1010 * GIB,
        },
    ];
    let s = RunSummary::finish(causes_input(close, Vec::new()));
    assert!(!s.explained);

    // over-explained: 15 GiB of causes against an 8 GiB shortfall
    let samples = vec![
        PreDeleteSample {
            at_ms: 0,
            data_bytes_used: 1000 * GIB,
        },
        PreDeleteSample {
            at_ms: 4_000,
            data_bytes_used: 1000 * GIB + 4 * GIB,
        },
    ];
    let s = RunSummary::finish(causes_input(samples, vec![hold("/c", Held::Found)]));
    assert!(!s.explained, "causes {:?}", s.causes);

    // Docker removed: the cause carries the delta, or 0 when unavailable
    let mut i = causes_input(Vec::new(), Vec::new());
    i.outcomes
        .push(removed(Kind::DockerImage, "sha256:aa", 38 * GIB));
    i.docker_delta_bytes = Some(38 * GIB);
    let s = RunSummary::finish(i);
    let d = s
        .causes
        .iter()
        .find(|c| c.kind == CauseKind::DockerDaemonDelta)
        .unwrap();
    assert_eq!(
        (d.bytes, d.source.as_str()),
        (38 * GIB as i64, "docker system df before/after")
    );
    let mut i = causes_input(Vec::new(), Vec::new());
    i.outcomes
        .push(removed(Kind::DockerImage, "sha256:aa", 38 * GIB));
    let s = RunSummary::finish(i);
    let d = s
        .causes
        .iter()
        .find(|c| c.kind == CauseKind::DockerDaemonDelta)
        .unwrap();
    assert_eq!(d.bytes, 0);
}

// ---- shared extents (ADR-004 outcome 3, spike 0.2.4)

fn ext(physical: u64) -> SharedExtent {
    SharedExtent {
        physical,
        length: 40 * MIB,
    }
}

/// 40 independent 40 MiB files plus five reflink clones of five of them, all in the removed set. Each clone
/// and its source carry the same shared extent.
fn spike_run() -> (Vec<Outcome>, ExtentAttribution) {
    let mut outcomes = Vec::new();
    let mut items = Vec::new();
    for n in 0..45_u64 {
        let p = format!("/f{n}");
        outcomes.push(removed(Kind::Cache, &p, 40 * MIB));
        let shared = if n < 5 {
            vec![ext(n)]
        } else if n >= 40 {
            vec![ext(n - 40)]
        } else {
            Vec::new()
        };
        items.push(ItemExtents {
            path: PathBuf::from(p),
            shared,
        });
    }
    (outcomes, ExtentAttribution::Measured(items))
}

#[test]
fn finish_should_count_a_shared_extent_once_across_the_run_and_report_shared_est_bytes() {
    let (outcomes, extents) = spike_run();
    let drop = 1_677_463_552_u64;
    let mut i = input(100 * GIB, 100 * GIB - drop, outcomes.clone());
    i.extents = extents;
    let s = RunSummary::finish(i);
    assert_eq!(s.attributed_bytes, 1_677_721_600);
    assert!(
        near(s.attribution_pct.unwrap(), 100.02, 0.005),
        "{:?}",
        s.attribution_pct
    );
    assert_eq!(s.shared_est_bytes, 0, "every twin is inside the set");
    assert_eq!(s.shared_line, None);
    assert!(s.attribution_line.contains("target >= 95%"));

    // mutant check: st_blocks alone reports 112.5 and would fail this test
    let mut i = input(100 * GIB, 100 * GIB - drop, outcomes);
    i.extents = ExtentAttribution::Unavailable(Unavailable::NotBtrfs);
    let plain = RunSummary::finish(i);
    assert!(near(plain.attribution_pct.unwrap(), 112.5, 0.05));
}

#[test]
fn an_extent_shared_with_a_file_outside_the_set_should_stay_counted_once_with_no_cause_kind_and_the_110_line_unchanged(
) {
    let outcomes = vec![
        removed(Kind::Cache, "/twinned", 3 * GIB),
        removed(Kind::Cache, "/plain", gib_f(109.5)),
    ];
    let extents = ExtentAttribution::Measured(vec![ItemExtents {
        path: PathBuf::from("/twinned"),
        shared: vec![SharedExtent {
            physical: 7,
            length: 3 * GIB,
        }],
    }]);
    let mut i = input(1000 * GIB, 900 * GIB, outcomes);
    i.extents = extents;
    let s = RunSummary::finish(i);
    assert_eq!(s.attributed_bytes, gib_f(112.5));
    assert_eq!(s.shared_est_bytes, 3 * GIB);
    assert_eq!(
        s.shared_line.as_deref(),
        Some("shared extents (may stay on disk): 3.0 GiB")
    );
    assert!(s
        .attribution_line
        .contains("attributed exceeds the counter drop"));
    // the causes enum has exactly three members: nothing names a shared extent
    assert!(s.causes.iter().all(|c| matches!(
        c.kind,
        CauseKind::SnapshotHeld | CauseKind::ConcurrentWriteDelta | CauseKind::DockerDaemonDelta
    )));
}

#[test]
fn attributed_112_5_against_drop_100_should_be_unexplained_with_no_shared_extent_cause_When_no_snapshot(
) {
    let outcomes = vec![removed(Kind::Cache, "/a", gib_f(112.5))];
    let s = RunSummary::finish(input(1000 * GIB, 900 * GIB, outcomes));
    assert!(!s.explained);
    assert!(s.causes.iter().all(|c| c.bytes == 0));
    assert!(s.snapshot_line.is_none());
}

#[test]
fn an_unavailable_probe_should_fall_back_to_st_blocks_with_the_note_and_fail_nothing() {
    let (outcomes, _) = spike_run();
    let mut i = input(100 * GIB, 98 * GIB, outcomes);
    i.extents = ExtentAttribution::Unavailable(Unavailable::IoctlUnsupported);
    let s = RunSummary::finish(i);
    assert_eq!(s.attributed_bytes, 45 * 40 * MIB);
    assert!(s
        .notes
        .iter()
        .any(|n| n == "shared extents not measured here: attribution may exceed the drop"));
    assert_eq!(s.shared_est_bytes, 0);
}

#[test]
fn top_unselected_should_hold_the_three_largest_kind_not_selected_outcomes() {
    let outcomes = vec![
        unselected("/a", 5),
        unselected("/b", 50),
        unselected("/c", 20),
        unselected("/d", 30),
        removed(Kind::Cache, "/e", GIB),
    ];
    let s = RunSummary::finish(input(10 * GIB, 9 * GIB, outcomes));
    let sizes: Vec<u64> = s.top_unselected.iter().map(|o| o.est_bytes).collect();
    assert_eq!(sizes, [50, 30, 20]);
    let _ = Path::new("/");
}

#[test]
fn non_btrfs_should_use_the_df_used_delta_and_say_so() {
    let mut before = FakeSpaceMeter::plain_snapshot("/", 500 * GIB, 1000 * GIB);
    before.fs_type = "ext4".into();
    let mut after = before.clone();
    after.used_bytes = 450 * GIB;
    let mut i = input(0, 0, vec![removed(Kind::Cache, "/a", 49 * GIB)]);
    i.fs_before = before;
    i.fs_after = Some(after);
    i.settled = None;
    let s = RunSummary::finish(i);
    assert_eq!(s.measured_drop, Some(50 * GIB as i64));
    assert_eq!(
        s.measured_line,
        "Measured drop in df used 50.0 GiB (df used delta; filesystem is ext4)"
    );
}

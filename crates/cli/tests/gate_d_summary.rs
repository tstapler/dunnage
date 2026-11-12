#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 4.3.3: the honest result messaging (UX-12, UX-14) over synthetic snapshots; no command runs.

mod support;

use dunnage_cli::apply::render_apply;
use dunnage_core::ports::ExtentAttribution;
use dunnage_core::snapshot_types::Settled;
use dunnage_core::summary::{DiskVerdict, FinishInput, RunSummary, StageTimings, WalkerCounters};
use dunnage_core::{Kind, SelectorId, Status};
use support::*;

const GIB: u64 = 1 << 30;

fn run_with(outcomes: Vec<dunnage_core::Outcome>, data_fell: u64, unallocated_after: u64) -> RunFx {
    let before = fs_btrfs().snapshot;
    let mut after = before.clone();
    let b = after.btrfs.as_mut().unwrap();
    b.data.bytes_used -= data_fell;
    b.unallocated = unallocated_after;
    after.avail_bytes += data_fell;
    let used_after = b.data.bytes_used;
    let summary = RunSummary::finish(FinishInput {
        fs_before: before,
        fs_after: Some(after),
        settled: Some(Settled {
            stable: true,
            data_bytes_used: Some(used_after),
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
        verdict_after: DiskVerdict::Warn,
        timings: StageTimings::default(),
        walker: WalkerCounters::default(),
        notes: Vec::new(),
    });
    RunFx {
        summary,
        selection: selection_of(&[SelectorId::Caches]),
    }
}

fn text(fx: &RunFx) -> String {
    render_apply(&fx.input(0), &pipe(100))
}

#[test]
fn summary_should_say_unallocated_did_not_grow_and_print_the_balance_command_When_data_fell_99_7_gib_and_unallocated_stayed(
) {
    let fx = run_with(
        vec![outcome(
            Kind::Cache,
            SelectorId::Caches,
            Some("/home/u/.cache/a"),
            Status::Removed,
            99.7,
        )],
        (99.7 * GIB as f64) as u64,
        (9.8 * GIB as f64) as u64,
    );
    let t = text(&fx);
    assert!(
        t.contains("freed data is still inside allocated chunks, so 'unallocated' did not grow"),
        "{t}"
    );
    assert!(t.contains("--apply --kinds btrfs"), "{t}");
}

#[test]
fn summary_should_print_the_gain_and_no_did_not_grow_note_When_a_balance_raised_unallocated_by_38_gib(
) {
    let fx = run_with(
        vec![outcome(
            Kind::Cache,
            SelectorId::Caches,
            Some("/home/u/.cache/a"),
            Status::Removed,
            99.7,
        )],
        (99.7 * GIB as f64) as u64,
        (9.8 * GIB as f64) as u64 + 38 * GIB,
    );
    let t = text(&fx);
    assert!(t.contains("+38.0 GiB"), "{t}");
    assert!(!t.contains("did not grow"), "{t}");
}

#[test]
fn summary_should_show_a_trim_as_its_own_line_and_not_in_freed_When_fstrim_trimmed_120_gib() {
    let mut o = outcome(
        Kind::BtrfsTrim,
        SelectorId::BtrfsTrim,
        Some("/"),
        Status::Removed,
        0.0,
    );
    o.trimmed_bytes = Some(120 * GIB);
    let fx = run_with(vec![o], 0, 9 * GIB);
    let t = text(&fx);
    assert!(
        t.contains("trimmed 120.0 GiB (fstrim; discarded blocks, not freed space)"),
        "{t}"
    );
    let first = t.lines().find(|l| l.starts_with("Freed")).unwrap_or("");
    assert!(first.starts_with("Freed 0 B"), "freed line: {first}\n{t}");
}

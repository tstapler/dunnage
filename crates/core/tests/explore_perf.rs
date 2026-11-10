#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Memory and throughput of the arena reducer. Events arrive shuffled inside windows of 2,048 (the order of a
//! threaded breadth-first walk); a global shuffle of 700,000 would park more than `PARK_MAX` events.
//! Memory and throughput of the arena reducer (Story 6.2.1; validation X-016, X-017). The throughput
//! figures are asserted in release builds; a debug build checks only a floor that catches a quadratic fold.

#[path = "common/explore.rs"]
mod ex;
use ex::*;

use std::ffi::OsString;
use std::time::Instant;

use disk_cleanup_core::explore::msg::{NodeEvent, ScanMode, NO_PARENT, PAINT_ID_BASE};
use disk_cleanup_core::explore::tree::ExploreTree;
use disk_cleanup_scan::index::DirInfo;

/// `n` directories, eight children each, basenames of 14 bytes.
fn synthetic(n: usize, base: u64, painted: bool) -> Vec<NodeEvent> {
    (0..n)
        .map(|i| {
            let kids = (1..=8).filter(|k| 8 * i + k < n).count() as u32;
            NodeEvent::Dir {
                id: base + i as u64,
                parent: if i == 0 {
                    NO_PARENT
                } else {
                    base + ((i - 1) / 8) as u64
                },
                name: OsString::from(if i == 0 {
                    "/r".to_string()
                } else {
                    format!("dir-{i:010}")
                }),
                dev: 1,
                info: DirInfo {
                    stats: stats_of(4096),
                    ..DirInfo::default()
                },
                subdirs: kids,
                from_cache: painted,
                painted,
            }
        })
        .collect()
}

#[test]
fn heap_bytes_per_directory_should_be_at_most_160_fresh_and_200_while_revalidating() {
    const N: usize = 100_000;
    let mut evs = synthetic(N, 0, false);
    shuffle_windowed(&mut evs, 1, 2048);
    let mut t = ExploreTree::new();
    reduce(&mut t, ScanMode::Fresh, 0, batches(evs, 1, 256));
    assert!(
        t.heap_bytes() / N <= 200,
        "mid-scan {} per directory",
        t.heap_bytes() / N
    );
    t.finish(false);
    let per = t.heap_bytes() / N;
    eprintln!("fresh after Finished: {per} bytes per directory");
    assert!(per <= 160, "fresh, after Finished: {per} per directory");
    assert_eq!(t.directories(), N);

    // a painted tree with its walker-id map and name index, half way through the live pass
    let mut t = ExploreTree::new();
    let mut paint = synthetic(N, PAINT_ID_BASE, true);
    shuffle_windowed(&mut paint, 2, 2048);
    reduce(&mut t, ScanMode::Paint, 0, batches(paint, 2, 256));
    let mut live = synthetic(N, 0, false);
    shuffle_windowed(&mut live, 3, 2048);
    let (a, b) = live.split_at(N / 2);
    reduce(&mut t, ScanMode::Revalidate, 0, batches(a.to_vec(), 3, 256));
    let per = t.heap_bytes() / N;
    eprintln!("revalidating: {per} bytes per directory");
    assert!(per <= 200, "revalidating: {per} per directory");
    reduce(&mut t, ScanMode::Revalidate, 0, batches(b.to_vec(), 4, 256));
    t.finish(false);
    let per = t.heap_bytes() / N;
    assert!(per <= 160, "after the live pass: {per} per directory");
}

#[test]
fn reducer_should_sustain_500k_events_per_second_and_apply_a_256_batch_under_2ms() {
    const N: usize = 700_000;
    let mut evs = synthetic(N, 0, false);
    shuffle_windowed(&mut evs, 7, 2048);
    let bs = batches(evs, 7, 256);
    let mut t = ExploreTree::new();
    let mut times = Vec::with_capacity(bs.len());
    let start = Instant::now();
    for b in bs {
        let s = Instant::now();
        t.apply_batch(0, ScanMode::Fresh, b).unwrap();
        times.push(s.elapsed());
    }
    let total = start.elapsed();
    let per_sec = N as f64 / total.as_secs_f64();
    times.sort();
    let p99 = times[times.len() * 99 / 100];
    eprintln!("{per_sec:.0} events/s, p99 per batch {p99:?}");
    if cfg!(debug_assertions) {
        assert!(per_sec >= 100_000.0, "{per_sec:.0} events/s");
    } else {
        assert!(per_sec >= 500_000.0, "{per_sec:.0} events/s");
        assert!(p99.as_secs_f64() < 0.002, "p99 {p99:?}");
    }
    assert_eq!(t.directories(), N);
}

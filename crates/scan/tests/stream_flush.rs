#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.1.2: a stalled or idle worker cannot strand an event (the 50 ms flush), and a stalled sink does not
//! reorder a parent behind its children by more than the flush bound.

mod stream_common;

use disk_cleanup_scan::index::{CollectRequest, DirEvent, StreamRequest, NO_PARENT};
use disk_cleanup_scan::walker::WalkerBackend;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use stream_common::{FakeFs, ROOT};

fn request(roots: &[PathBuf], threads: usize) -> StreamRequest<'_> {
    StreamRequest::new(
        CollectRequest {
            roots,
            cross_fs: false,
            deadline: None,
            threads,
        },
        Arc::new(AtomicBool::new(false)),
    )
}

#[test]
fn parent_event_should_reach_the_sink_within_100ms_When_its_worker_blocks_2s_in_the_lister() {
    let mut fake = FakeFs::new(2, 1);
    fake.delay = Box::new(|n| {
        if n == 1 {
            Duration::from_secs(2)
        } else {
            Duration::ZERO
        }
    });
    let created = Arc::new(Mutex::new(None::<Instant>));
    let c2 = Arc::clone(&created);
    fake.on_finish = Box::new(move |id, _| {
        if id == 0 {
            *c2.lock().unwrap() = Some(Instant::now());
        }
    });
    let arrived = Mutex::new(None::<Instant>);
    let sink = |events: Vec<DirEvent>| {
        if events.iter().any(|e| e.id() == 0) {
            arrived.lock().unwrap().get_or_insert_with(Instant::now);
        }
    };
    let roots = vec![PathBuf::from(ROOT)];
    WalkerBackend::default()
        .stream(&request(&roots, 2), &sink, &fake)
        .unwrap();
    let created = created.lock().unwrap().unwrap();
    let late = arrived.lock().unwrap().unwrap().duration_since(created);
    assert!(
        late < Duration::from_millis(100),
        "the parent event waited {late:?} while its worker was stalled"
    );
}

#[test]
fn every_parent_event_should_precede_its_first_child_or_arrive_within_100ms_When_1000000_children_and_a_stalled_sink(
) {
    const KIDS: u64 = 1_000_000;
    let mut fake = FakeFs::new(KIDS + 1, KIDS);
    let created: Arc<Vec<AtomicU64>> = Arc::new((0..=KIDS).map(|_| AtomicU64::new(0)).collect());
    let epoch = Instant::now();
    let c2 = Arc::clone(&created);
    fake.on_finish = Box::new(move |id, _| {
        c2[id as usize].store(epoch.elapsed().as_nanos() as u64 + 1, Ordering::Relaxed);
    });
    let arrival: Vec<AtomicU64> = (0..=KIDS).map(|_| AtomicU64::new(0)).collect();
    let parents: Vec<AtomicU64> = (0..=KIDS).map(|_| AtomicU64::new(NO_PARENT)).collect();
    let sink = |events: Vec<DirEvent>| {
        std::thread::sleep(Duration::from_millis(10));
        let now = epoch.elapsed().as_nanos() as u64 + 1;
        for e in &events {
            arrival[e.id() as usize].store(now, Ordering::Relaxed);
            parents[e.id() as usize].store(e.parent(), Ordering::Relaxed);
        }
    };
    let roots = vec![PathBuf::from(ROOT)];
    WalkerBackend::default()
        .stream(&request(&roots, 8), &sink, &fake)
        .unwrap();
    let load = |v: &Vec<AtomicU64>, i: usize| v[i].load(Ordering::Relaxed);
    assert!(
        (0..=KIDS as usize).all(|i| load(&arrival, i) != 0),
        "every id has an arrival"
    );
    let first_child = (1..=KIDS as usize)
        .map(|i| load(&arrival, i))
        .min()
        .unwrap();
    let root_arrival = load(&arrival, 0);
    let root_wait = root_arrival.saturating_sub(load(&created, 0));
    assert!(
        root_arrival <= first_child || root_wait <= 100_000_000,
        "the parent arrived {root_wait} ns after it was created and after its first child"
    );
    assert!(
        (1..=KIDS as usize).all(|i| load(&parents, i) == 0),
        "every child names the root"
    );
}

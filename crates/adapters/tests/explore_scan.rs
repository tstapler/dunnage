#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.3.1 Task 6.3.1a: `WalkerScanSource` over the real walker and over scripted backends.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use disk_cleanup_adapters::explore::scan_source::{WalkerScanSource, QUEUE_DEPTH};
use disk_cleanup_core::explore::msg::{NodeEvent, ScanMode, ScanMsg, StopCause, NO_PARENT};
use disk_cleanup_core::explore::ports::{ScanHandle, ScanOpts, ScanSource};
use disk_cleanup_core::explore::tree::ExploreTree;
use disk_cleanup_scan::index::{
    Backend, CollectRequest, Collected, DirEvent, DirInfo, StreamRequest, StreamSummary,
};
use disk_cleanup_scan::walker::WalkerBackend;

type Sink<'a> = &'a (dyn Fn(Vec<DirEvent>) + Sync);
type Script = dyn Fn(&StreamRequest<'_>, Sink<'_>) -> io::Result<StreamSummary> + Send + Sync;

/// A backend whose `collect_streaming` is a closure.
struct Scripted(Box<Script>);

impl Backend for Scripted {
    fn name(&self) -> &'static str {
        "scripted"
    }
    fn extents_exact(&self) -> bool {
        false
    }
    fn collect(&self, _req: &CollectRequest<'_>) -> disk_cleanup_scan::Result<Collected> {
        Ok(Collected::default())
    }
    fn collect_streaming(
        &self,
        req: &StreamRequest<'_>,
        sink: Sink<'_>,
    ) -> io::Result<StreamSummary> {
        (self.0)(req, sink)
    }
}

fn scripted(
    f: impl Fn(&StreamRequest<'_>, Sink<'_>) -> io::Result<StreamSummary> + Send + Sync + 'static,
) -> WalkerScanSource {
    WalkerScanSource::new(Scripted(Box::new(f)))
}

/// `StreamSummary` is `#[non_exhaustive]`: start from the default and set fields.
fn summary(f: impl FnOnce(&mut StreamSummary)) -> StreamSummary {
    let mut s = StreamSummary::default();
    f(&mut s);
    s
}

fn dir_event(id: u64) -> DirEvent {
    DirEvent::Dir {
        id,
        parent: if id == 0 { NO_PARENT } else { 0 },
        name: if id == 0 {
            "/r".into()
        } else {
            format!("d{id}").into()
        },
        dev: 1,
        info: DirInfo::default(),
        subdirs: 0,
        from_cache: false,
        painted: false,
    }
}

fn recv_until_finished(h: &ScanHandle) -> (Vec<ScanMsg>, ScanMsg) {
    let mut seen = Vec::new();
    loop {
        match h.rx.recv_timeout(Duration::from_secs(60)) {
            Ok(m @ (ScanMsg::Finished(_) | ScanMsg::SourceFailed(_))) => return (seen, m),
            Ok(m) => seen.push(m),
            Err(e) => panic!("no terminal message: {e:?}"),
        }
    }
}

fn root() -> Vec<PathBuf> {
    vec![PathBuf::from("/r")]
}

/// 2,000 directories under a tempdir, a few files each.
fn fixture_2000() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    for i in 0..50 {
        for j in 0..40 {
            let d = base.join(format!("a{i}")).join(format!("b{j}"));
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("f"), vec![1u8; 100 + i * j]).unwrap();
        }
    }
    (tmp, base)
}

#[test]
fn walker_source_totals_should_equal_collect_When_2000_directory_fixture_is_folded() {
    let (_tmp, base) = fixture_2000();
    let roots = vec![base.clone()];
    let wanted = WalkerBackend::default()
        .collect(&CollectRequest {
            roots: &roots,
            cross_fs: false,
            deadline: None,
            threads: 4,
        })
        .unwrap();
    assert_eq!(wanted.dirs.len(), 2051);
    let (inodes, bytes) = wanted.dirs.iter().fold((0, 0), |(i, b), (_, d)| {
        (i + d.stats.inodes, b + d.stats.allocated_bytes)
    });

    let h = WalkerScanSource::new(WalkerBackend::default()).start(&roots, &ScanOpts::default());
    let mut tree = ExploreTree::new();
    let (batches, last) = recv_until_finished(&h);
    for m in batches {
        let ScanMsg::Batch {
            generation,
            mode,
            events,
        } = m
        else {
            panic!("unexpected message {m:?}");
        };
        assert_eq!(mode, ScanMode::Fresh);
        tree.apply_batch(generation, mode, events).unwrap();
    }
    let ScanMsg::Finished(summary) = last else {
        panic!("{last:?}")
    };
    tree.finish(summary.truncated);
    let root = tree.roots().next().unwrap();
    assert_eq!(tree.directories(), 2051);
    assert_eq!(tree.total(root).inodes, inodes);
    assert_eq!(tree.total(root).allocated_bytes, bytes);
    assert!(!summary.truncated);
    assert_eq!(summary.stop, StopCause::None);
    assert_eq!(summary.dirs_listed, wanted.dirs_listed);
    assert_eq!(summary.errors, wanted.errors);
}

#[test]
fn walker_source_should_block_the_walk_on_a_full_queue_When_the_consumer_stops_receiving() {
    let returned = Arc::new(AtomicUsize::new(0));
    let r2 = Arc::clone(&returned);
    let src = scripted(move |_req, sink| {
        for i in 0..1000 {
            sink(vec![dir_event(i)]);
            r2.fetch_add(1, Ordering::SeqCst);
        }
        Ok(StreamSummary::default())
    });
    let h = src.start(&root(), &ScanOpts::default());
    std::thread::sleep(Duration::from_millis(400));
    // The queue holds QUEUE_DEPTH batches; the next send is blocked and has not returned.
    assert_eq!(QUEUE_DEPTH, 64);
    assert_eq!(returned.load(Ordering::SeqCst), QUEUE_DEPTH);
    // Draining lets it finish.
    let (batches, last) = recv_until_finished(&h);
    assert_eq!(batches.len(), 1000);
    assert!(matches!(last, ScanMsg::Finished(_)));
}

#[test]
fn walker_source_should_end_the_thread_as_a_cancel_When_the_receiver_is_dropped_while_a_send_blocks(
) {
    let exited = Arc::new(AtomicBool::new(false));
    let saw_cancel = Arc::new(AtomicBool::new(false));
    let (e2, c2) = (Arc::clone(&exited), Arc::clone(&saw_cancel));
    let src = scripted(move |req, sink| {
        for i in 0..1_000_000 {
            sink(vec![dir_event(i)]);
            if req.cancel.load(Ordering::SeqCst) {
                c2.store(true, Ordering::SeqCst);
                break;
            }
        }
        e2.store(true, Ordering::SeqCst);
        Ok(StreamSummary::default())
    });
    let h = src.start(&root(), &ScanOpts::default());
    std::thread::sleep(Duration::from_millis(200));
    assert!(!exited.load(Ordering::SeqCst), "blocked on the full queue");
    let t = Instant::now();
    drop(h);
    while !exited.load(Ordering::SeqCst) {
        assert!(t.elapsed() < Duration::from_secs(5), "thread did not end");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        saw_cancel.load(Ordering::SeqCst),
        "the failed send was a cancel"
    );
}

#[test]
fn quit_should_not_wait_for_the_walk_When_the_backend_ignores_cancel() {
    let exited = Arc::new(AtomicBool::new(false));
    let e2 = Arc::clone(&exited);
    let src = scripted(move |_req, _sink| {
        std::thread::sleep(Duration::from_secs(60));
        e2.store(true, Ordering::SeqCst);
        Ok(StreamSummary::default())
    });
    let t = Instant::now();
    let h = src.start(&root(), &ScanOpts::default());
    h.cancel();
    h.detach();
    drop(h);
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "quit took {:?}",
        t.elapsed()
    );
    assert!(
        !exited.load(Ordering::SeqCst),
        "the thread is still running, unjoined"
    );
}

#[test]
fn walker_source_should_finish_truncated_within_1s_When_a_well_behaved_walk_is_cancelled() {
    let src = scripted(|req, sink| {
        let mut i = 0;
        while !req.cancel.load(Ordering::SeqCst) {
            sink(vec![dir_event(i)]);
            i += 1;
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(summary(|s| s.truncated = true))
    });
    let h = src.start(&root(), &ScanOpts::default());
    std::thread::sleep(Duration::from_millis(50));
    let t = Instant::now();
    h.cancel();
    let (_, last) = recv_until_finished(&h);
    assert!(t.elapsed() < Duration::from_secs(1));
    let ScanMsg::Finished(s) = last else { panic!() };
    assert!(s.truncated);
    assert_eq!(s.stop, StopCause::Cancelled);
}

#[test]
fn walker_source_should_report_the_deadline_as_the_stop_When_max_seconds_ends_the_walk() {
    let src = scripted(|req, _sink| {
        let deadline = req
            .collect
            .deadline
            .expect("max_seconds becomes the deadline");
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(summary(|s| s.truncated = true))
    });
    let opts = ScanOpts {
        max_seconds: Some(1),
        generation: 7,
        ..ScanOpts::default()
    };
    let h = src.start(&root(), &opts);
    let (_, last) = recv_until_finished(&h);
    let ScanMsg::Finished(s) = last else { panic!() };
    assert_eq!(
        (s.generation, s.truncated, s.stop),
        (7, true, StopCause::Deadline)
    );
}

#[test]
fn walker_source_should_hold_the_save_gate_and_pass_notes_and_counters() {
    let src = scripted(|req, _sink| {
        assert!(req.save.load(Ordering::SeqCst), "the gate starts set");
        Ok(summary(|s| {
            s.errors = 3;
            s.dirs_listed = 5;
            s.dirs_cached = 6;
            s.notes = vec!["a note".into()];
        }))
    });
    let h = src.start(&root(), &ScanOpts::default());
    let (_, last) = recv_until_finished(&h);
    let ScanMsg::Finished(s) = last else { panic!() };
    assert_eq!((s.errors, s.dirs_listed, s.dirs_cached), (3, 5, 6));
    assert_eq!(s.notes, vec!["a note".to_string()]);
}

#[test]
fn walker_source_should_report_a_backend_error_as_source_failed() {
    let src = scripted(|_r, _s| Err(io::Error::from_raw_os_error(2)));
    let h = src.start(&root(), &ScanOpts::default());
    let (_, last) = recv_until_finished(&h);
    let ScanMsg::SourceFailed(text) = last else {
        panic!()
    };
    assert!(text.starts_with("scan failed: "), "{text}");
}

const PANIC_CHILD: &str = "scan_thread_panic_child";

#[test]
fn scan_thread_panic_child() {
    if std::env::var_os("DC_SCAN_PANIC_CHILD").is_none() {
        return;
    }
    let src = scripted(|_r, _s| panic!("boom with a path /secret/dir"));
    let h = src.start(&root(), &ScanOpts::default());
    match h.rx.recv_timeout(Duration::from_secs(10)) {
        Ok(ScanMsg::SourceFailed(t)) if t == "scan thread panicked" => {
            println!("CHILD_OK:{PANIC_CHILD}");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(matches!(
        h.rx.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Disconnected | RecvTimeoutError::Timeout)
    ));
}

#[test]
fn walker_source_should_turn_a_panic_into_source_failed_and_write_nothing_to_stderr() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", PANIC_CHILD, "--nocapture", "--test-threads=1"])
        .env("DC_SCAN_PANIC_CHILD", "1")
        .output()
        .unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        stdout.contains(&format!("CHILD_OK:{PANIC_CHILD}")),
        "{stdout}{stderr}"
    );
    assert!(
        !stderr.contains("panicked") && !stderr.contains("boom"),
        "stderr: {stderr}"
    );
}

#[test]
fn walker_source_should_cancel_a_real_walk_and_keep_what_it_visited() {
    let (_tmp, base) = fixture_2000();
    let roots = vec![base];
    let h = WalkerScanSource::new(WalkerBackend::default()).start(&roots, &ScanOpts::default());
    h.cancel();
    let (batches, last) = recv_until_finished(&h);
    let ScanMsg::Finished(s) = last else { panic!() };
    // Cancelled before it began, or midway: either way it is truncated by the user, never a failure.
    assert!(!s.truncated || s.stop == StopCause::Cancelled);
    for m in batches {
        let ScanMsg::Batch { events, .. } = m else {
            panic!()
        };
        assert!(events.iter().all(|e| matches!(
            e,
            NodeEvent::Dir { .. }
                | NodeEvent::Unreadable { .. }
                | NodeEvent::Boundary { .. }
                | NodeEvent::Gone { .. }
        )));
    }
}

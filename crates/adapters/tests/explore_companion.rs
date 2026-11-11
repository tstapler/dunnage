#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.5.4: the two hooks of `WalkerScanSource` the explore wiring uses: a companion that shares the
//! scan's channel, and an activity counter that is back to 0 before the final message arrives.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

use disk_cleanup_adapters::explore::scan_source::WalkerScanSource;
use disk_cleanup_core::explore::msg::ScanMsg;
use disk_cleanup_core::explore::ports::{ScanOpts, ScanSource};
use disk_cleanup_scan::walker::WalkerBackend;

#[test]
fn the_companion_should_send_on_the_scan_channel_and_the_counter_should_drop_before_finished() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
    let activity = Arc::new(AtomicUsize::new(0));
    let seen_roots = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen_roots);
    let source = WalkerScanSource::new(WalkerBackend::default())
        .with_activity(Arc::clone(&activity))
        .with_companion(Arc::new(move |roots, tx| {
            recorder.lock().unwrap().extend(roots.iter().cloned());
            tx.send(ScanMsg::PreviewFailed("from the companion".into()))
                .unwrap();
        }));
    let handle = source.start(&[dir.path().to_path_buf()], &ScanOpts::default());
    let (mut companion, mut finished) = (false, false);
    while !finished {
        match handle.rx.recv_timeout(Duration::from_secs(10)) {
            Ok(ScanMsg::PreviewFailed(t)) => {
                assert_eq!(t, "from the companion");
                companion = true;
            }
            Ok(ScanMsg::Finished(_)) => {
                assert_eq!(
                    activity.load(Ordering::SeqCst),
                    0,
                    "the walk still counted as running when its final message arrived"
                );
                finished = true;
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                panic!("no Finished message")
            }
        }
    }
    assert!(companion, "the companion's message never arrived");
    assert_eq!(*seen_roots.lock().unwrap(), vec![dir.path().to_path_buf()]);
}

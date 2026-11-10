//! `settle`: `syncfs` once, then poll `data.bytes_used` until two equal samples or the timeout (Task 1.2.2b,
//! ADR-004). btrfs frees space lazily, so a read right after the last delete under-reports the drop; a
//! timeout returns the last value with `stable: false`, which the summary labels `provisional`.

use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use disk_cleanup_core::Settled;

/// Flush the filesystem holding `path`. Best effort: a failure only makes the wait longer.
#[cfg(target_os = "linux")]
pub fn sync_filesystem(path: &Path) {
    if let Ok(f) = fs::File::open(path) {
        let _ = rustix::fs::syncfs(&f);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn sync_filesystem(_path: &Path) {}

/// `sync` runs once; `read` returns the counter (`None` when the filesystem has no such counter, which is
/// stable by definition) and is called at least twice before a stable answer.
pub fn settle(
    sync: impl FnOnce(),
    mut read: impl FnMut() -> Option<u64>,
    timeout: Duration,
    poll: Duration,
) -> Settled {
    sync();
    let start = Instant::now();
    let mut samples = 0_u32;
    let mut last: Option<u64> = None;
    loop {
        let used = read();
        samples += 1;
        if used.is_none() || (samples > 1 && used == last) {
            return settled(true, used, samples, start);
        }
        last = used;
        if start.elapsed() >= timeout {
            return settled(false, used, samples, start);
        }
        thread::sleep(poll.min(timeout.saturating_sub(start.elapsed())));
    }
}

fn settled(stable: bool, data_bytes_used: Option<u64>, samples: u32, start: Instant) -> Settled {
    Settled {
        stable,
        data_bytes_used,
        samples,
        waited_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

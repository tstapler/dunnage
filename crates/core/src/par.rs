//! Order-preserving parallel map on scoped threads (std only). Discovery is read-only and I/O bound
//! (child processes, `lstat`, directory reads), so a few threads cut wall time without changing results:
//! slot `i` of the output always holds `f(&items[i])`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Upper bound on worker threads for one `par_map`; discovery nests (kinds, then repos, then worktrees), so
/// each level stays modest.
pub const MAX_WORKERS: usize = 8;

/// `items.iter().map(f).collect()` run on up to `workers` threads. A panic in `f` propagates to the caller.
pub fn par_map<T, R, F>(items: &[T], workers: usize, f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let n = workers.min(items.len());
    if n <= 1 {
        return items.iter().map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..n {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(i) else { break };
                let r = f(item);
                *slots[i]
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(r);
            });
        }
    });
    slots
        .into_iter()
        .filter_map(|m| {
            m.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        })
        .collect()
}

/// Worker count for process-and-I/O bound work. A constant, not `available_parallelism`: that reads
/// `/sys/devices/system/cpu/online`, which the plan-run canary (no access outside HOME and an allowlist) forbids.
pub fn default_workers() -> usize {
    MAX_WORKERS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn par_map_should_keep_input_order_whatever_the_finish_order() {
        let items: Vec<u64> = (0..64).collect();
        let out = par_map(&items, 8, |i| {
            std::thread::sleep(std::time::Duration::from_millis(64 - *i));
            *i * 2
        });
        assert_eq!(out, items.iter().map(|i| i * 2).collect::<Vec<_>>());
    }

    #[test]
    fn par_map_should_equal_the_sequential_map_for_one_worker_and_empty_input() {
        assert_eq!(par_map(&[1, 2, 3], 1, |i| i + 1), vec![2, 3, 4]);
        assert!(par_map::<u8, u8, _>(&[], 4, |i| *i).is_empty());
    }
}

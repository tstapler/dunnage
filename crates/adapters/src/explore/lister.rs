//! `RealDirLister` (Task 6.3.3b): the largest files of one directory by allocated bytes, without reading a
//! 400,000-entry directory into memory. One pass over `read_dir` keeps a heap of at most `top_n` entries and
//! sums the rest; the cancel flag is checked before every entry, so a cancel stops it within one `lstat`.
//! Read-only: `read_dir` and `lstat` only.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use dunnage_core::explore::msg::{FileRow, FileRows};
use dunnage_core::explore::ports::DirLister;

/// Lists with the real filesystem.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealDirLister;

/// Ordered by allocated bytes, then by name so that of two equal files the earlier name ranks higher.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Ranked {
    allocated: u64,
    name: Reverse<OsString>,
    size: u64,
    second_link: bool,
}

impl RealDirLister {
    /// The listing, or the errno of the directory that could not be read. A cancel or a read error in the
    /// middle returns what was gathered with `partial` set.
    ///
    /// A hard-linked inode (`nlink > 1`) seen again in this listing is marked `second_link`; the table of
    /// seen inodes is capped at `top_n`, so a listing with more hard links than that marks fewer.
    pub fn list(&self, dir: &Path, top_n: usize, cancel: &AtomicBool) -> Result<FileRows, i32> {
        let entries = std::fs::read_dir(dir).map_err(|e| {
            e.raw_os_error()
                .unwrap_or(rustix::io::Errno::IO.raw_os_error())
        })?;
        let mut heap: BinaryHeap<Reverse<Ranked>> = BinaryHeap::with_capacity(top_n.min(1 << 16));
        let mut seen: HashSet<(u64, u64)> = HashSet::new();
        let (mut smaller_count, mut smaller_bytes) = (0u64, 0u64);
        let mut partial = false;
        for entry in entries {
            if cancel.load(Ordering::SeqCst) {
                partial = true;
                break;
            }
            let Ok(entry) = entry else {
                partial = true;
                break;
            };
            // `DirEntry::metadata` is an `lstat`: a symlink is listed with its own size.
            let Ok(meta) = entry.metadata() else {
                continue; // gone since readdir
            };
            if meta.is_dir() {
                continue;
            }
            let second_link = meta.nlink() > 1 && {
                let key = (meta.dev(), meta.ino());
                if seen.contains(&key) {
                    true
                } else {
                    if seen.len() < top_n {
                        seen.insert(key);
                    }
                    false
                }
            };
            let allocated = meta.blocks() * 512;
            if top_n == 0 {
                smaller_count += 1;
                smaller_bytes += allocated;
                continue;
            }
            let ranked = Ranked {
                allocated,
                name: Reverse(entry.file_name()),
                size: meta.len(),
                second_link,
            };
            if heap.len() < top_n {
                heap.push(Reverse(ranked));
            } else if let Some(mut lowest) = heap.peek_mut() {
                if ranked > lowest.0 {
                    let out = std::mem::replace(&mut lowest.0, ranked);
                    smaller_count += 1;
                    smaller_bytes += out.allocated;
                } else {
                    smaller_count += 1;
                    smaller_bytes += ranked.allocated;
                }
            }
        }
        let rows = heap
            .into_sorted_vec()
            .into_iter()
            .map(|Reverse(r)| FileRow {
                name: r.name.0,
                allocated_bytes: r.allocated,
                size_bytes: r.size,
                second_link: r.second_link,
            })
            .collect();
        Ok(FileRows {
            rows,
            smaller_count,
            smaller_bytes,
            partial,
        })
    }
}

impl DirLister for RealDirLister {
    fn list_files(&self, dir: &Path, top_n: usize, cancel: &AtomicBool) -> FileRows {
        self.list(dir, top_n, cancel).unwrap_or(FileRows {
            partial: true,
            ..FileRows::default()
        })
    }
}

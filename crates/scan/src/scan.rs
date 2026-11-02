//! Directory-tree walk that aggregates [`Stats`] into depth-limited buckets.

use crate::estimate::{EntryInfo, Kind, Stats};
use crate::{Error, Result};
use ignore::WalkBuilder;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub root: PathBuf,
    /// Entries deeper than this many components below `root` are rolled into their ancestor at
    /// this depth. Depth 1 gives one bucket per child of `root`.
    pub depth: usize,
    /// Count paths that .gitignore / .ignore would hide (node_modules, target/, ...).
    pub include_ignored: bool,
    /// Descend into other mounts and btrfs subvolumes (distinct `st_dev`).
    pub cross_fs: bool,
    pub max_duration: Option<Duration>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Bucket {
    /// Relative to the scan root; "." holds files directly under root.
    pub path: PathBuf,
    pub stats: Stats,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanReport {
    pub root: PathBuf,
    pub buckets: Vec<Bucket>,
    pub total: Stats,
    /// True when `max_duration` cut the walk short, so figures are partial.
    pub truncated: bool,
    pub errors: u64,
    pub elapsed_ms: u128,
}

/// Which bucket an entry belongs to: directories own themselves, files belong to their parent.
pub fn bucket_for(rel: &Path, is_dir: bool, depth: usize) -> PathBuf {
    let comps: Vec<_> = rel.components().collect();
    let take = if is_dir {
        comps.len().min(depth)
    } else {
        comps.len().saturating_sub(1).min(depth)
    };
    if take == 0 {
        return PathBuf::from(".");
    }
    comps[..take].iter().collect()
}

fn kind_of(ft: std::fs::FileType) -> Kind {
    if ft.is_dir() {
        Kind::Dir
    } else if ft.is_symlink() {
        Kind::Symlink
    } else if ft.is_file() {
        Kind::File
    } else {
        Kind::Other
    }
}

pub fn scan(opts: &ScanOptions) -> Result<ScanReport> {
    std::fs::symlink_metadata(&opts.root).map_err(|source| Error::Io {
        path: opts.root.clone(),
        source,
    })?;
    let start = Instant::now();
    let mut builder = WalkBuilder::new(&opts.root);
    builder
        .hidden(false)
        .follow_links(false)
        .same_file_system(!opts.cross_fs);
    if opts.include_ignored {
        builder.standard_filters(false);
    }

    let mut buckets: BTreeMap<PathBuf, Stats> = BTreeMap::new();
    let mut seen_links: HashSet<(u64, u64)> = HashSet::new();
    let (mut errors, mut truncated, mut visited) = (0u64, false, 0u64);

    for item in builder.build() {
        visited += 1;
        if visited % 1024 == 0 && opts.max_duration.is_some_and(|d| start.elapsed() > d) {
            truncated = true;
            break;
        }
        let Ok(entry) = item else {
            errors += 1;
            continue;
        };
        let Ok(md) = entry.metadata() else {
            errors += 1;
            continue;
        };
        let kind = kind_of(md.file_type());
        let first_link =
            kind == Kind::Dir || md.nlink() <= 1 || seen_links.insert((md.dev(), md.ino()));
        let rel = entry
            .path()
            .strip_prefix(&opts.root)
            .unwrap_or(entry.path());
        let info = EntryInfo {
            kind,
            size: md.len(),
            allocated: md.blocks() * 512,
            name_len: entry.file_name().len() as u64,
            first_link,
            counts_dir_entry: entry.depth() > 0,
        };
        buckets
            .entry(bucket_for(rel, kind == Kind::Dir, opts.depth))
            .or_default()
            .add_entry(&info);
    }

    let mut total = Stats::default();
    for s in buckets.values() {
        total.merge(s);
    }
    Ok(ScanReport {
        root: opts.root.clone(),
        buckets: buckets
            .into_iter()
            .map(|(path, stats)| Bucket { path, stats })
            .collect(),
        total,
        truncated,
        errors,
        elapsed_ms: start.elapsed().as_millis(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn bucket_assignment() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(bucket_for(&p(""), true, 1), p("."));
        assert_eq!(bucket_for(&p("a"), true, 1), p("a"));
        assert_eq!(bucket_for(&p("a/b/c"), true, 1), p("a"));
        assert_eq!(bucket_for(&p("a/b/c"), true, 2), p("a/b"));
        assert_eq!(bucket_for(&p("top.txt"), false, 1), p("."));
        assert_eq!(bucket_for(&p("a/f.txt"), false, 1), p("a"));
        assert_eq!(bucket_for(&p("a/b/f.txt"), false, 1), p("a"));
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("metascope-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn counts_a_small_tree_and_honours_gitignore() {
        let root = tmp("tree");
        fs::create_dir_all(root.join("repo/.git")).unwrap();
        fs::create_dir_all(root.join("repo/node_modules/x")).unwrap();
        fs::write(root.join("repo/.gitignore"), "node_modules\n").unwrap();
        fs::write(root.join("repo/a.txt"), "hello").unwrap();
        fs::write(root.join("repo/node_modules/x/i.js"), "1").unwrap();

        let mut opts = ScanOptions {
            root: root.clone(),
            depth: 1,
            include_ignored: false,
            cross_fs: false,
            max_duration: None,
        };
        let hidden = scan(&opts).unwrap();
        opts.include_ignored = true;
        let full = scan(&opts).unwrap();
        fs::remove_dir_all(&root).unwrap();

        // node_modules, node_modules/x and i.js are the three extra inodes.
        assert_eq!(full.total.inodes - hidden.total.inodes, 3);
        assert!(!full.truncated && full.errors == 0);
        let repo = full
            .buckets
            .iter()
            .find(|b| b.path == Path::new("repo"))
            .unwrap();
        assert_eq!(repo.stats.files, 3); // .gitignore, a.txt, i.js
    }

    #[test]
    fn hard_links_count_one_inode() {
        let root = tmp("links");
        fs::write(root.join("a"), "data").unwrap();
        fs::hard_link(root.join("a"), root.join("b")).unwrap();
        let opts = ScanOptions {
            root: root.clone(),
            depth: 1,
            include_ignored: true,
            cross_fs: false,
            max_duration: None,
        };
        let r = scan(&opts).unwrap();
        fs::remove_dir_all(&root).unwrap();
        assert_eq!((r.total.files, r.total.dir_entries), (1, 2));
    }
}

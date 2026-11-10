//! The seam between data collection and attribution: every [`Backend`] produces the same
//! per-directory counters, and `scan` classifies them without caring where they came from.

use crate::estimate::Stats;
use crate::{Error, Result};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

/// Counters for one directory key: its own inode/entry plus its direct non-directory children.
/// Child directories are separate keys.
#[derive(Debug, Default, Clone)]
pub struct DirInfo {
    pub stats: Stats,
    pub mtime: i64,
    pub has_package_json: bool,
    pub has_cargo_toml: bool,
    pub git_file: bool,
}

impl DirInfo {
    pub fn merge(&mut self, o: &DirInfo) {
        self.stats.merge(&o.stats);
        self.mtime = self.mtime.max(o.mtime);
        self.has_package_json |= o.has_package_json;
        self.has_cargo_toml |= o.has_cargo_toml;
        self.git_file |= o.git_file;
    }

    /// Records marker files by name; `is_regular_file` guards against directories named alike.
    pub fn note_file_name(&mut self, name: &[u8], is_regular_file: bool) {
        if !is_regular_file {
            return;
        }
        match name {
            b"package.json" => self.has_package_json = true,
            b"Cargo.toml" => self.has_cargo_toml = true,
            b".git" => self.git_file = true,
            _ => {}
        }
    }
}

pub struct CollectRequest<'a> {
    pub roots: &'a [PathBuf],
    pub cross_fs: bool,
    pub deadline: Option<Instant>,
    pub threads: usize,
}

#[derive(Debug, Default)]
pub struct Collected {
    pub dirs: Vec<(PathBuf, DirInfo)>,
    pub errors: u64,
    pub truncated: bool,
    /// Directories whose entries were read from the filesystem.
    pub dirs_listed: u64,
    /// Directories answered from the incremental cache without a readdir.
    pub dirs_cached: u64,
    /// Non-fatal problems worth showing (cache write failed, fell back from O_NOATIME, ...).
    pub notes: Vec<String>,
}

/// `parent` of a root directory event.
pub const NO_PARENT: u64 = u64::MAX;

/// Ids of `painted` events start here, so they never collide with the dense ids of the live walk.
pub const PAINT_ID_BASE: u64 = 1 << 62;

/// Default `StreamRequest::batch_max`.
pub const DEFAULT_BATCH_MAX: usize = 256;

/// One per-directory fact delivered while a walk runs. Every id the walk hands out receives exactly one
/// terminal event (`Dir`, `Unreadable`, `Boundary` or `Gone`) unless the walk is stopped first.
/// Roots carry their full path as `name`, every other event its basename.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum DirEvent {
    /// A listed (or cached) directory: `info` is what `collect` returns for the same path.
    Dir {
        id: u64,
        parent: u64,
        name: OsString,
        /// `st_dev` of the directory; 0 when unknown (`painted` events and replayed `collect` results).
        dev: u64,
        info: DirInfo,
        /// Number of child directories that will each get their own event.
        subdirs: u32,
        /// Answered from the incremental cache without a readdir.
        from_cache: bool,
        /// A provisional record painted from the cache before the live walk, replaced by the live event.
        painted: bool,
    },
    /// The directory could not be stat'ed or opened.
    Unreadable {
        id: u64,
        parent: u64,
        name: OsString,
        errno: i32,
    },
    /// A directory on another device, not entered because `cross_fs` is off.
    Boundary {
        id: u64,
        parent: u64,
        name: OsString,
        dev: u64,
    },
    /// The directory vanished, or is no longer a directory, between its parent's listing and its turn.
    Gone {
        id: u64,
        parent: u64,
        name: OsString,
    },
}

impl DirEvent {
    pub fn id(&self) -> u64 {
        match self {
            Self::Dir { id, .. }
            | Self::Unreadable { id, .. }
            | Self::Boundary { id, .. }
            | Self::Gone { id, .. } => *id,
        }
    }

    pub fn parent(&self) -> u64 {
        match self {
            Self::Dir { parent, .. }
            | Self::Unreadable { parent, .. }
            | Self::Boundary { parent, .. }
            | Self::Gone { parent, .. } => *parent,
        }
    }
}

/// Input of [`Backend::collect_streaming`]: a [`CollectRequest`] plus what streaming adds.
#[non_exhaustive]
pub struct StreamRequest<'a> {
    pub collect: CollectRequest<'a>,
    /// Set to stop the walk; the walk returns `truncated` and keeps what it visited.
    pub cancel: Arc<AtomicBool>,
    /// Emit the cache's records as `painted` events before the live walk (one cache load for the whole run).
    pub paint_from_cache: bool,
    /// The cache is saved only while this is set; clearing it during the save aborts the save.
    pub save: Arc<AtomicBool>,
    /// Largest batch handed to the sink.
    pub batch_max: usize,
}

impl<'a> StreamRequest<'a> {
    pub fn new(collect: CollectRequest<'a>, cancel: Arc<AtomicBool>) -> Self {
        Self {
            collect,
            cancel,
            paint_from_cache: false,
            save: Arc::new(AtomicBool::new(true)),
            batch_max: DEFAULT_BATCH_MAX,
        }
    }
}

/// What a streaming walk reports at its end (every event was already delivered).
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct StreamSummary {
    pub truncated: bool,
    pub errors: u64,
    pub dirs_listed: u64,
    pub dirs_cached: u64,
    pub notes: Vec<String>,
}

pub trait Backend {
    fn name(&self) -> &'static str;
    /// True when `Stats::extents` is read from the filesystem rather than estimated.
    fn extents_exact(&self) -> bool;
    fn collect(&self, req: &CollectRequest<'_>) -> Result<Collected>;

    /// Delivers the walk as batches of [`DirEvent`] while it runs. The default replays [`Backend::collect`]:
    /// one `Dir` event per entry, ids dense from 0, parents before children.
    fn collect_streaming(
        &self,
        req: &StreamRequest<'_>,
        sink: &(dyn Fn(Vec<DirEvent>) + Sync),
    ) -> io::Result<StreamSummary> {
        let collected = self.collect(&req.collect).map_err(into_io)?;
        replay(&collected, req, sink);
        Ok(StreamSummary {
            truncated: collected.truncated,
            errors: collected.errors,
            dirs_listed: collected.dirs_listed,
            dirs_cached: collected.dirs_cached,
            notes: collected.notes,
        })
    }
}

fn into_io(e: Error) -> io::Error {
    match e {
        Error::Io { source, .. } => source,
        other => io::Error::other(other.to_string()),
    }
}

fn replay(collected: &Collected, req: &StreamRequest<'_>, sink: &(dyn Fn(Vec<DirEvent>) + Sync)) {
    let roots = req.collect.roots;
    let mut order: Vec<usize> = (0..collected.dirs.len()).collect();
    order.sort_by_key(|&i| collected.dirs[i].0.components().count());
    let ids: HashMap<&Path, u64> = order
        .iter()
        .enumerate()
        .map(|(id, &i)| (collected.dirs[i].0.as_path(), id as u64))
        .collect();
    let parent_of = |p: &Path| -> u64 {
        if roots.iter().any(|r| r == p) {
            return NO_PARENT;
        }
        p.ancestors()
            .skip(1)
            .find_map(|a| ids.get(a).copied())
            .unwrap_or(NO_PARENT)
    };
    let mut children: HashMap<u64, u32> = HashMap::new();
    for &i in &order {
        let parent = parent_of(&collected.dirs[i].0);
        if parent != NO_PARENT {
            *children.entry(parent).or_default() += 1;
        }
    }
    let mut batch = Vec::with_capacity(req.batch_max.min(order.len()));
    for (id, &i) in order.iter().enumerate() {
        let (path, info) = &collected.dirs[i];
        let parent = parent_of(path);
        let name = if parent == NO_PARENT {
            path.as_os_str().to_owned()
        } else {
            path.file_name().map(OsString::from).unwrap_or_default()
        };
        batch.push(DirEvent::Dir {
            id: id as u64,
            parent,
            name,
            dev: 0,
            info: info.clone(),
            subdirs: children.get(&(id as u64)).copied().unwrap_or(0),
            from_cache: false,
            painted: false,
        });
        if batch.len() >= req.batch_max.max(1) {
            sink(std::mem::take(&mut batch));
        }
    }
    if !batch.is_empty() {
        sink(batch);
    }
}

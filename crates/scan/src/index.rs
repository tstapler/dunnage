//! The seam between data collection and attribution: every [`Backend`] produces the same
//! per-directory counters, and `scan` classifies them without caring where they came from.

use crate::estimate::Stats;
use crate::Result;
use std::path::PathBuf;
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

pub trait Backend {
    fn name(&self) -> &'static str;
    /// True when `Stats::extents` is read from the filesystem rather than estimated.
    fn extents_exact(&self) -> bool;
    fn collect(&self, req: &CollectRequest<'_>) -> Result<Collected>;
}

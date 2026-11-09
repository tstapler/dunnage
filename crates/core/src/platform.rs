//! Host facts the `SystemProbe` port returns.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    Linux,
    Macos,
}

/// A yes/no answer that can be "could not tell". Callers fail closed on `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tri {
    Running,
    NotRunning,
    Unknown,
}

/// One row of the mount table (`/proc/self/mountinfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub mount_point: PathBuf,
    pub dev: u64,
    pub mnt_id: u64,
}

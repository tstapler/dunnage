//! Side-effecting adapters (filesystem, processes, config).

pub mod command;
pub mod config;
pub mod delete;
pub mod dirops;
pub mod discover_build;
pub mod discover_cache;
pub mod mtime_probe;
pub mod probe;
pub mod snapshots;
pub mod space_meter;
pub mod statvfs;
pub mod steam;
#[cfg(feature = "test-support")]
pub mod testing;
pub mod treestat;

pub use delete::delete_tree;
pub use dirops::{
    open_parent, open_verified_root, DirOps, RealDirOps, VerifiedParent, VerifiedRoot, DELETE_BATCH,
};

pub use command::RealCommandRunner;
pub use probe::RealProbe;
pub use snapshots::{SnapshotInfo, SnapshotLayout};
pub use space_meter::RealSpaceMeter;

//! Side-effecting adapters (filesystem, processes, config).

pub mod audit;
pub mod audit_verify;
mod audit_verify_explorer;
pub use audit_verify_explorer::own_dirs;
pub mod command;
pub mod command_confined;
pub mod config;
pub mod delete;
pub mod dirops;
pub mod discover_build;
pub mod discover_cache;
pub mod docker_cli;
pub mod explore;
pub mod fiemap;
pub mod gate_stamp;
pub mod lock;
pub mod mtime_probe;
pub mod probe;
pub mod settle;
pub mod signals;
pub mod snapshots;
pub mod space_meter;
pub mod state;
pub mod status;
pub mod statvfs;
pub mod steam;
pub mod targets;
#[cfg(feature = "test-support")]
pub mod testing;
pub mod treestat;

pub use delete::delete_tree;
pub use dirops::{
    fs_type_of, mount_id_of, open_parent, open_verified_root, DirOps, RealDirOps, VerifiedParent,
    VerifiedRoot, DELETE_BATCH,
};

pub use command::RealCommandRunner;
pub use docker_cli::DockerCli;
pub use explore::lister::RealDirLister;
pub use explore::signals::RealTermSignals;
pub use fiemap::FiemapProbe;
pub use probe::RealProbe;
pub use snapshots::{SnapshotInfo, SnapshotLayout};
pub use space_meter::RealSpaceMeter;

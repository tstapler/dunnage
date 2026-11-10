//! Side-effecting adapters (filesystem, processes, config).

pub mod command;
pub mod delete;
pub mod dirops;
pub mod probe;
#[cfg(feature = "test-support")]
pub mod testing;
pub mod treestat;

pub use delete::delete_tree;
pub use dirops::{
    open_parent, open_verified_root, DirOps, RealDirOps, VerifiedParent, VerifiedRoot, DELETE_BATCH,
};

pub use command::RealCommandRunner;
pub use probe::RealProbe;

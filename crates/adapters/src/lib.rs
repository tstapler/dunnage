//! Side-effecting adapters (filesystem, processes, config).

pub mod delete;
pub mod dirops;

pub use delete::delete_tree;
pub use dirops::{
    open_parent, open_verified_root, DirOps, RealDirOps, VerifiedParent, VerifiedRoot, DELETE_BATCH,
};

//! Transport-independent logic for metascope. The CLI (and a future ratatui
//! TUI) consume these types; nothing here prints or parses arguments.

pub mod btrfs;
pub mod btrfs_tree;
pub mod cache;
pub mod estimate;
pub mod health;
pub mod index;
pub mod scan;
pub mod walker;

/// Errors surfaced by the library.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reading {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: expected an unsigned integer, got {value:?}")]
    Parse {
        path: std::path::PathBuf,
        value: String,
    },
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

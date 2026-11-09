//! The four directories the tool reads or writes outside the scan roots. Resolution from the environment
//! is Story 1.1.6; this is only the value.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    /// Canonical.
    pub home: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
    pub config: PathBuf,
}

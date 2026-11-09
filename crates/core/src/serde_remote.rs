//! Serde mirrors for the frozen plain-data types (Story 0.1.3), which must not change to gain derives.

use serde::{Deserialize, Serialize};

use crate::delete_types::Freed;
use crate::ids::{AllocatedBytes, ApparentBytes, Identity};

#[derive(Serialize, Deserialize)]
#[serde(remote = "Identity")]
pub(crate) struct IdentityDef {
    dev: u64,
    ino: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "AllocatedBytes")]
pub(crate) struct AllocatedBytesDef(pub u64);

#[derive(Serialize, Deserialize)]
#[serde(remote = "ApparentBytes")]
pub(crate) struct ApparentBytesDef(pub u64);

#[derive(Serialize, Deserialize)]
#[serde(remote = "Freed")]
pub(crate) struct FreedDef {
    #[serde(with = "AllocatedBytesDef")]
    attributed_bytes: AllocatedBytes,
    #[serde(with = "ApparentBytesDef")]
    apparent_bytes: ApparentBytes,
    entries_removed: u64,
    inodes_freed: u64,
}

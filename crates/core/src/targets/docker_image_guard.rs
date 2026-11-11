//! Docker image guards (Stories 4.1.2 and 4.1.3): pure decisions over facts the adapter read.
//! Reference checks fail closed: an image whose container count Docker did not print is treated as used.

use crate::outcome::SkipReason;

/// What the adapter knows about one image id (the rows of `docker images` that share it, merged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageFacts {
    /// Sum over the rows; `None` when any row printed `N/A`.
    pub containers: Option<u32>,
    /// A container of `docker ps -a` names this image by tag, repository or id.
    pub named_by_container: bool,
    /// `None` for a locally built image (`<none>`).
    pub digest: Option<String>,
    /// `None` when the creation time did not parse.
    pub created_epoch: Option<i64>,
}

fn referenced(f: &ImageFacts) -> bool {
    f.named_by_container || f.containers != Some(0)
}

/// A dangling image: only a reference stops it.
#[must_use]
pub fn decide_dangling_image(f: &ImageFacts) -> Option<SkipReason> {
    referenced(f).then_some(SkipReason::ImageReferenced)
}

/// A tagged image the owner named with `docker-images`: referenced, then built here, then too recent. An
/// unknown age is too recent.
#[must_use]
pub fn decide_unused_image(f: &ImageFacts, min_age_secs: u64, now: i64) -> Option<SkipReason> {
    if referenced(f) {
        return Some(SkipReason::ImageReferenced);
    }
    if f.digest.is_none() {
        return Some(SkipReason::LocalOnlyImage);
    }
    let min = i64::try_from(min_age_secs).unwrap_or(i64::MAX);
    match f.created_epoch {
        Some(t) if now.saturating_sub(t) >= min => None,
        _ => Some(SkipReason::TooRecent),
    }
}

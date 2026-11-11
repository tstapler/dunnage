//! Docker volume guards (Story 4.1.4): the pure decision for one volume. An anonymous volume left by
//! `docker run postgres` has no referencing container once the container is gone, so "unreferenced" alone
//! never makes a volume removable: it must also be old, opted in by class, small and readable, with no
//! database signature, unless the owner names it with `--volume`.

use crate::outcome::SkipReason;
use crate::selector::DockerVolumes;

/// Above this a volume is treated as a database whatever it holds.
pub const DATABASE_SIZE_BYTES: u64 = 1 << 30;

/// The reason text of the compose guard (`VolumeReferenced`).
pub const COMPOSE_DETAIL: &str = "compose project present";

/// What the invoking user could list at the top level of the mount point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Contents {
    /// `Permission denied`, no such directory (a Docker Desktop VM), or any other read error.
    Unreadable,
    Names(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeFacts {
    pub name: String,
    pub anonymous: bool,
    /// The first container that uses the volume, when one does.
    pub used_by: Option<String>,
    /// The compose labels name a project whose working directory exists on this machine.
    pub compose_project_present: bool,
    /// From `system df -v`, rounded: approximate. `None` is unknown and fails closed.
    pub approx_size_bytes: Option<u64>,
    /// `None` when `CreatedAt` was missing or did not parse: too recent.
    pub created_epoch: Option<i64>,
    pub contents: Contents,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeOpts<'a> {
    /// `--docker-volumes`; `None` lists everything and removes nothing.
    pub class: Option<DockerVolumes>,
    /// `--volume <name>`, exact names.
    pub named: &'a [String],
    pub min_age_secs: u64,
    pub now: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeVerdict {
    /// `None` is allow.
    pub reason: Option<SkipReason>,
    /// The `<x>` or `<signature>` slot of the reason text.
    pub detail: Option<String>,
}

impl VolumeVerdict {
    fn allow() -> VolumeVerdict {
        VolumeVerdict {
            reason: None,
            detail: None,
        }
    }

    fn skip(reason: SkipReason, detail: Option<String>) -> VolumeVerdict {
        VolumeVerdict {
            reason: Some(reason),
            detail,
        }
    }
}

/// The database signature among a volume's top-level names: `PG_VERSION`, `ibdata1`, `*.rdb`,
/// `mongod.lock`, `*.sqlite`.
#[must_use]
pub fn database_signature(names: &[String]) -> Option<String> {
    names
        .iter()
        .find(|n| {
            matches!(n.as_str(), "PG_VERSION" | "ibdata1" | "mongod.lock")
                || n.ends_with(".rdb")
                || n.ends_with(".sqlite")
        })
        .cloned()
}

/// Order: referenced, too recent, class opt-in, then the two heuristics (database-like, unreadable) which
/// `--volume <name>` lifts. `VolumeReferenced`, `TooRecent` and `OptInRequired` are never lifted.
#[must_use]
pub fn decide_volume(f: &VolumeFacts, o: &VolumeOpts<'_>) -> VolumeVerdict {
    if let Some(who) = &f.used_by {
        return VolumeVerdict::skip(SkipReason::VolumeReferenced, Some(who.clone()));
    }
    if f.compose_project_present {
        return VolumeVerdict::skip(
            SkipReason::VolumeReferenced,
            Some(COMPOSE_DETAIL.to_string()),
        );
    }
    let min = i64::try_from(o.min_age_secs).unwrap_or(i64::MAX);
    if !f
        .created_epoch
        .is_some_and(|t| o.now.saturating_sub(t) >= min)
    {
        return VolumeVerdict::skip(SkipReason::TooRecent, None);
    }
    let opted_in = match o.class {
        Some(DockerVolumes::Anonymous) => f.anonymous,
        Some(DockerVolumes::Named) => !f.anonymous,
        None => false,
    };
    if !opted_in {
        return VolumeVerdict::skip(SkipReason::OptInRequired, None);
    }
    if o.named.iter().any(|n| *n == f.name) {
        return VolumeVerdict::allow();
    }
    let names = match &f.contents {
        Contents::Unreadable => return VolumeVerdict::skip(SkipReason::ContentsUnknown, None),
        Contents::Names(n) => n,
    };
    if let Some(sig) = database_signature(names) {
        return VolumeVerdict::skip(SkipReason::LooksLikeDatabase, Some(sig));
    }
    match f.approx_size_bytes {
        Some(b) if b > DATABASE_SIZE_BYTES => VolumeVerdict::skip(
            SkipReason::LooksLikeDatabase,
            Some("larger than 1 GiB".to_string()),
        ),
        Some(_) => VolumeVerdict::allow(),
        None => VolumeVerdict::skip(SkipReason::ContentsUnknown, None),
    }
}

/// The evidence every listed volume row carries so the owner decides with data: size (approximate, the
/// daemon rounds it to four digits), creation time, mount point and class.
#[must_use]
pub fn evidence_text(
    approx_size_bytes: Option<u64>,
    created_at: Option<&str>,
    mountpoint: &str,
    anonymous: bool,
) -> String {
    let size = approx_size_bytes.map_or_else(
        || "size unknown".to_string(),
        |b| format!("~{}", si_text(b)),
    );
    format!(
        "{size}, created {}, {mountpoint}, {}",
        created_at.unwrap_or("unknown"),
        if anonymous { "anonymous" } else { "named" }
    )
}

/// Decimal SI, as Docker prints sizes (`1.049MB`).
fn si_text(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let b = bytes as f64;
    for (unit, name) in [(1e12, "TB"), (1e9, "GB"), (1e6, "MB"), (1e3, "kB")] {
        if b >= unit {
            return format!("{:.3}{name}", b / unit);
        }
    }
    format!("{bytes}B")
}

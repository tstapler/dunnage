//! Pure decisions of the cargo-aware kinds: `cargo-artifact` (a part of a cargo target, [`decide_cargo_part`])
//! and `cargo-home` (an aged crate or checkout below `CARGO_HOME`, [`decide_cargo_home`]). Like
//! `targets::build_dir`, they run at plan time and again just before the delete, over facts a probe in
//! `adapters` gathered, and the one `Allowed` they return is the witness `delete_tree` needs.

use std::time::Duration;

use crate::allowed::Allowed;
use crate::guard::{decide_path_common, Facts, GuardDecision};
use crate::ids::Identity;
use crate::outcome::SkipReason;
use crate::roots::RootSet;
use crate::selector::{format_age, CARGO_HOME_FLOOR_SECS};
use crate::targets::build_dir::{effective_min_age, BuildGuard};

/// What cargo writes into `CACHEDIR.TAG` (the Cache Directory Tagging spec's signature).
pub const CACHEDIR_TAG_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";

/// Detail on the skip of a part whose target no longer looks like cargo's.
pub const DETAIL_NOT_A_CARGO_TARGET: &str =
    "no cargo CACHEDIR.TAG or .rustc_info.json in target, or no Cargo.toml next to it";

/// A directory named `target` next to a `Cargo.toml` is a real cargo target when cargo's own tag or its
/// `rustc` info cache is inside. Either file alone suffices: old cargo wrote only the info cache, a pruned
/// target may have lost it.
pub fn is_cargo_target(tag_has_signature: bool, has_rustc_info: bool) -> bool {
    tag_has_signature || has_rustc_info
}

/// Whether the tag file content is cargo's.
pub fn tag_has_signature(content: &[u8]) -> bool {
    content.starts_with(CACHEDIR_TAG_SIGNATURE.as_bytes())
}

/// `--cargo-profile`: empty offers every profile; a name matches the whole relative profile (`debug`,
/// `x86_64-unknown-linux-gnu/debug`) or its last component (`debug`).
pub fn profile_selected(profile: &str, wanted: &[String]) -> bool {
    wanted.is_empty()
        || wanted
            .iter()
            .any(|w| w == profile || profile.rsplit('/').next() == Some(w.as_str()))
}

/// Fresh facts about one part of a cargo target.
#[derive(Debug, Clone)]
pub struct CargoPartFacts {
    pub path: Facts,
    pub planned: Identity,
    /// The target still has cargo's tag or info cache, a profile with `.fingerprint` or `deps` above the
    /// part, and a regular `Cargo.toml` next to `target`.
    pub structure_ok: bool,
    /// Newest mtime that decides staleness (Unix seconds): the part's own contents for `incremental`, the
    /// whole profile's activity for the others. `None` when it could not be established.
    pub activity_mtime: Option<i64>,
}

/// `min_age` is the flag as given; the 60 minute build-dir floor is applied here.
pub fn decide_cargo_part(
    facts: &CargoPartFacts,
    roots: &RootSet,
    now: i64,
    min_age: Duration,
) -> BuildGuard {
    let allowed = match decide_path_common(&facts.path, roots) {
        GuardDecision::Allow(a) => a,
        GuardDecision::Skip(r) => return skip(r, None),
    };
    if facts.path.ident != facts.planned {
        return skip(SkipReason::IdentityChanged, None);
    }
    if !facts.structure_ok {
        return skip(SkipReason::IdentityChanged, Some(DETAIL_NOT_A_CARGO_TARGET));
    }
    young_check(
        facts.activity_mtime,
        now,
        effective_min_age(min_age).0,
        allowed,
    )
}

/// Fresh facts about one crate or checkout below `CARGO_HOME`.
#[derive(Debug, Clone)]
pub struct CargoHomeFacts {
    pub path: Facts,
    pub planned: Identity,
    /// Newest mtime inside (Unix seconds); `None` when it could not be established.
    pub newest_mtime: Option<i64>,
}

/// `max(min_age, 24h)`: an entry younger than a day is never removed.
pub fn cargo_home_min_age(flag: Duration) -> Duration {
    flag.max(Duration::from_secs(CARGO_HOME_FLOOR_SECS))
}

pub fn decide_cargo_home(
    facts: &CargoHomeFacts,
    roots: &RootSet,
    now: i64,
    min_age: Duration,
) -> BuildGuard {
    let allowed = match decide_path_common(&facts.path, roots) {
        GuardDecision::Allow(a) => a,
        GuardDecision::Skip(r) => return skip(r, None),
    };
    if facts.path.ident != facts.planned {
        return skip(SkipReason::IdentityChanged, None);
    }
    young_check(
        facts.newest_mtime,
        now,
        cargo_home_min_age(min_age),
        allowed,
    )
}

fn skip(reason: SkipReason, detail: Option<&str>) -> BuildGuard {
    BuildGuard::Skip {
        reason,
        detail: detail.map(str::to_string),
    }
}

fn young_check(newest: Option<i64>, now: i64, effective: Duration, allowed: Allowed) -> BuildGuard {
    let Some(newest) = newest else {
        return skip(
            SkipReason::TooRecent,
            Some("the newest modification time inside could not be read"),
        );
    };
    let floor = i64::try_from(effective.as_secs()).unwrap_or(i64::MAX);
    if now.saturating_sub(newest) < floor {
        // the catalogue spells the build-dir floor "60m", not format_age's "1h"
        let age = if effective.as_secs() == 3600 {
            "60m".to_string()
        } else {
            format_age(effective.as_secs())
        };
        return skip(SkipReason::TooRecent, Some(&format!("younger than {age}")));
    }
    BuildGuard::Allow(allowed)
}

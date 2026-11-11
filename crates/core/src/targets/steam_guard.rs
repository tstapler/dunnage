//! Steam guard (Story 4.2.2): the pure decision for an orphan prefix, an orphan shader cache or an
//! uninstalled-game remnant, made at plan time and again just before the delete. Order: common path checks,
//! identity and orphan status, age (newest mtime inside, never the directory's own), saves. Whether Steam
//! runs and whether a library is mounted are whole-kind facts and are decided before any item (`KindGate`).

use std::time::Duration;

use crate::allowed::Allowed;
use crate::guard::{decide_path_common, Facts, GuardDecision};
use crate::ids::Identity;
use crate::kind::Kind;
use crate::outcome::SkipReason;
use crate::roots::RootSet;
use crate::selector::Discard;

/// Directories of a Proton prefix that hold a game's saves, relative to `SAVES_BASE`.
pub const SAVE_DIRS: [&str; 4] = ["Saved Games", "Documents", "My Documents", "AppData"];
/// Where `SAVE_DIRS` live inside a prefix.
pub const SAVES_BASE: &str = "pfx/drive_c/users/steamuser";

/// The detail of a `TooRecent` verdict made without a readable modification time.
pub const DETAIL_MTIME_UNKNOWN: &str = "the newest modification time inside could not be read";
/// The detail of the `IdentityChanged` verdict for an item a manifest now names.
pub const DETAIL_NOW_INSTALLED: &str = "an appmanifest now names it";

/// Fresh facts about one Steam item. `planned` is the identity taken at discovery.
#[derive(Debug, Clone)]
pub struct SteamGuardFacts {
    pub path: Facts,
    pub planned: Identity,
    /// No manifest of any library names the item (an app id for prefixes and shader caches, an `installdir`
    /// for remnants), as of the library read that goes with these facts.
    pub still_orphan: bool,
    /// Newest mtime inside (Unix seconds); `None` when it could not be established.
    pub newest_mtime: Option<i64>,
    /// The first of `SAVE_DIRS` that holds a file; only looked for in prefixes.
    pub saves_dir: Option<String>,
}

#[derive(Debug)]
pub enum SteamGuard {
    Allow(Allowed),
    Skip {
        reason: SkipReason,
        detail: Option<String>,
    },
}

impl SteamGuard {
    fn skip(reason: SkipReason) -> SteamGuard {
        SteamGuard::Skip {
            reason,
            detail: None,
        }
    }

    fn skip_with(reason: SkipReason, detail: impl Into<String>) -> SteamGuard {
        SteamGuard::Skip {
            reason,
            detail: Some(detail.into()),
        }
    }
}

/// `min_age` is the flag as given (30 days by default); `saves` is `--steam-saves discard`.
pub fn decide_steam(
    facts: &SteamGuardFacts,
    roots: &RootSet,
    now: i64,
    min_age: Duration,
    saves: Option<Discard>,
) -> SteamGuard {
    let allowed = match decide_path_common(&facts.path, roots) {
        GuardDecision::Allow(a) => a,
        GuardDecision::Skip(r) => return SteamGuard::skip(r),
    };
    if facts.path.ident != facts.planned {
        return SteamGuard::skip(SkipReason::IdentityChanged);
    }
    if !facts.still_orphan {
        return SteamGuard::skip_with(SkipReason::IdentityChanged, DETAIL_NOW_INSTALLED);
    }
    let Some(newest) = facts.newest_mtime else {
        return SteamGuard::skip_with(SkipReason::TooRecent, DETAIL_MTIME_UNKNOWN);
    };
    let floor = i64::try_from(min_age.as_secs()).unwrap_or(i64::MAX);
    if now.saturating_sub(newest) < floor {
        return SteamGuard::skip(SkipReason::TooRecent);
    }
    if facts.path.kind == Kind::SteamPrefix && saves.is_none() {
        if let Some(dir) = &facts.saves_dir {
            return SteamGuard::skip_with(SkipReason::MayHoldSaves, dir.clone());
        }
    }
    SteamGuard::Allow(allowed)
}

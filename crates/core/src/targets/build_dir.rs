//! Build-dir guard (Story 3.2.1): the pure decision made at plan time and again just before deletion, over
//! facts a probe in `adapters` gathered. Order: common path checks, identity and marker, age floor,
//! reproducibility. The one `Allowed` it can return is the witness `delete_tree` needs.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::allowed::Allowed;
use crate::guard::{decide_path_common, Facts, GuardDecision};
use crate::ids::Identity;
use crate::kind::Kind;
use crate::outcome::SkipReason;
use crate::ports::RunState;
use crate::roots::RootSet;
use crate::selector::{format_age, BUILD_DIR_FLOOR_SECS};

/// A build dir is never removed below this age; `--min-age` and `--stale-days` cannot lower it.
pub const BUILD_DIR_MIN_AGE: Duration = Duration::from_secs(BUILD_DIR_FLOOR_SECS);

/// Plan header line when the flag was below the floor.
pub const FLOOR_RAISED_NOTE: &str = "build-dirs: --min-age raised to the 60m floor";

/// What `--min-age` means for build dirs: `max(flag, 60m)`, and whether the floor was the binding one.
pub fn effective_min_age(flag: Duration) -> (Duration, bool) {
    if flag < BUILD_DIR_MIN_AGE {
        (BUILD_DIR_MIN_AGE, true)
    } else {
        (flag, false)
    }
}

/// Whether `install` can recreate a `node_modules` (E-8). A cargo `target/` is always `Reproducible`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reproducibility {
    Reproducible,
    /// A top-level or `.bin` entry is a symlink whose target lies outside the project (`npm link`).
    LinkedOutside,
    NoLockfile,
    /// The probe stopped before looking; the guard fails closed.
    NotChecked,
}

pub const DETAIL_LINKED_OUTSIDE: &str = "an npm-linked package points outside the project";
pub const DETAIL_NO_LOCKFILE: &str = "no lockfile next to node_modules";

/// Fresh facts about one build dir. `path` carries the `lstat`/`fstat` identity (the device is the entry's
/// own `st_dev`, never the mount table's); `planned` is the identity taken at discovery.
#[derive(Debug, Clone)]
pub struct BuildFacts {
    pub path: Facts,
    pub planned: Identity,
    /// The sibling `package.json` or `Cargo.toml` still marks it as a build dir.
    pub marker_present: bool,
    /// Newest mtime inside (Unix seconds); `None` when it could not be established.
    pub newest_mtime: Option<i64>,
    pub reproducibility: Reproducibility,
}

#[derive(Debug)]
pub enum BuildGuard {
    Allow(Allowed),
    Skip {
        reason: SkipReason,
        detail: Option<String>,
    },
}

impl BuildGuard {
    fn skip(reason: SkipReason) -> BuildGuard {
        BuildGuard::Skip {
            reason,
            detail: None,
        }
    }

    fn skip_with(reason: SkipReason, detail: impl Into<String>) -> BuildGuard {
        BuildGuard::Skip {
            reason,
            detail: Some(detail.into()),
        }
    }
}

/// `min_age` is the flag as given; the floor is applied here.
pub fn decide_build_dir(
    facts: &BuildFacts,
    roots: &RootSet,
    now: i64,
    min_age: Duration,
) -> BuildGuard {
    let allowed = match decide_path_common(&facts.path, roots) {
        GuardDecision::Allow(a) => a,
        GuardDecision::Skip(r) => return BuildGuard::skip(r),
    };
    if facts.path.ident != facts.planned || !facts.marker_present {
        return BuildGuard::skip(SkipReason::IdentityChanged);
    }
    let (effective, floored) = effective_min_age(min_age);
    let Some(newest) = facts.newest_mtime else {
        return BuildGuard::skip_with(
            SkipReason::TooRecent,
            "the newest modification time inside could not be read",
        );
    };
    let floor = i64::try_from(effective.as_secs()).unwrap_or(i64::MAX);
    if now.saturating_sub(newest) < floor {
        let tail = if floored { " (build-dir floor)" } else { "" };
        // the catalogue spells the floor "60m", not format_age's "1h"
        let age = if effective == BUILD_DIR_MIN_AGE {
            "60m".to_string()
        } else {
            format_age(effective.as_secs())
        };
        return BuildGuard::skip_with(
            SkipReason::TooRecent,
            format!("younger than --min-age {age}{tail}"),
        );
    }
    if facts.path.kind == Kind::NodeModules {
        match facts.reproducibility {
            Reproducibility::Reproducible => {}
            Reproducibility::LinkedOutside => {
                return BuildGuard::skip_with(SkipReason::NotReproducible, DETAIL_LINKED_OUTSIDE)
            }
            Reproducibility::NoLockfile => {
                return BuildGuard::skip_with(SkipReason::NotReproducible, DETAIL_NO_LOCKFILE)
            }
            Reproducibility::NotChecked => {
                return BuildGuard::skip_with(SkipReason::NotReproducible, "not checked")
            }
        }
    }
    BuildGuard::Allow(allowed)
}

/// A running build is never deleted (Story 3.2.2). `Idle` is the only state that lets a candidate through.
pub fn run_state_skip(state: RunState) -> Option<SkipReason> {
    match state {
        RunState::Idle => None,
        RunState::Running => Some(SkipReason::RunningBuild),
        RunState::Unknown => Some(SkipReason::RunStateUnknown),
    }
}

/// Header line when `--keep-newest 0` offers the newest copy of every duplicate set too.
pub const KEEP_NEWEST_ZERO_WARNING: &str =
    "WARNING: --keep-newest 0 also offers the newest copy of every duplicate set";

/// `--keep-newest N`: how many copies of each duplicate set survive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepNewest(pub usize);

impl KeepNewest {
    pub fn warning(self) -> Option<&'static str> {
        (self.0 == 0).then_some(KEEP_NEWEST_ZERO_WARNING)
    }
}

/// One build dir as the duplicate planner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterInput {
    pub path: PathBuf,
    pub kind: Kind,
    /// `None` when the lockfile could not be parsed: the dir is in no cluster.
    pub lockfile_hash: Option<u64>,
    /// Newest mtime inside (Unix seconds). `None` (unknown recency) ranks as newest, so it is kept.
    pub newest_mtime: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// In no duplicate set: judged by the age rules only.
    Unclustered,
    /// Survives as one of the newest; `of` is the size of its duplicate set.
    Kept { of: usize },
    /// An older duplicate: eligible (the guards still apply).
    Offered,
}

/// The note on a `KeptAsNewest` row.
pub fn kept_note(of: usize) -> String {
    format!("kept as newest of {of} duplicates")
}

/// Ranks every duplicate set (same kind and lockfile hash, two or more members) by true recency and keeps
/// the `keep` newest of each. Ties break by path so the plan is deterministic. The result is parallel to
/// `inputs`.
pub fn plan_keep_newest(inputs: &[ClusterInput], keep: KeepNewest) -> Vec<Placement> {
    let mut sets: BTreeMap<(Kind, u64), Vec<usize>> = BTreeMap::new();
    for (i, m) in inputs.iter().enumerate() {
        if let Some(h) = m.lockfile_hash {
            sets.entry((m.kind, h)).or_default().push(i);
        }
    }
    let mut out = vec![Placement::Unclustered; inputs.len()];
    for mut idx in sets.into_values().filter(|s| s.len() > 1) {
        idx.sort_by(|&a, &b| {
            let rank = |i: usize| inputs[i].newest_mtime.unwrap_or(i64::MAX);
            rank(b)
                .cmp(&rank(a))
                .then_with(|| inputs[a].path.cmp(&inputs[b].path))
        });
        let of = idx.len();
        for (pos, i) in idx.into_iter().enumerate() {
            out[i] = if pos < keep.0 {
                Placement::Kept { of }
            } else {
                Placement::Offered
            };
        }
    }
    out
}

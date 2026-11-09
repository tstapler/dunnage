//! `Candidate`: one discovered cleanable thing, with its locator and per-kind facts.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ids::{AllocatedBytes, AppId, CandidateId, DockerId, Identity};
use crate::kind::{selector_of, Kind, SelectorId};
use crate::serde_remote::IdentityDef;

/// Where a candidate lives: exactly one of a path, a docker object or a Steam path (no both-`None` or
/// both-`Some` pair is representable).
///
/// ```compile_fail
/// use disk_cleanup_core::Locator;
/// // a locator with no place at all does not exist
/// let _ = Locator::Neither;
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Locator {
    Path(PathBuf),
    Docker(DockerId),
    Steam(AppId, PathBuf),
}

/// DC-owned mirror of what metascope knows about a build directory, so `metascope_core` types stay inside
/// `adapters`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildDirFacts {
    pub flavor: BuildDirFlavor,
    pub group: PathBuf,
    /// Seconds since the directory's own mtime (under-reports recent activity inside).
    pub age_secs: u64,
    pub lockfile_hash: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuildDirFlavor {
    NodeModules,
    CargoTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeFacts {
    pub main_repo: PathBuf,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheFacts {
    pub label: String,
    /// The apt cache sits behind `cache-apt`, every other cache behind `caches`.
    pub apt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DockerFacts {
    DanglingImage { image_id: String },
    BuildCache,
    UnusedImage { image_id: String, tags: Vec<String> },
    Volume { name: String, anonymous: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SteamFacts {
    Prefix { app_id: AppId },
    ShaderCache { app_id: AppId },
    Remnant { install_dir: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BtrfsFacts {
    Balance { usage_pct: u8 },
    Trim,
}

/// Per-kind facts. The kind of a candidate is derived from these, never stored beside them, so a cache kind
/// with docker facts does not type-check:
///
/// ```compile_fail
/// use disk_cleanup_core::{DockerFacts, KindFacts};
/// let _ = KindFacts::Cache(DockerFacts::BuildCache);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "kebab-case")]
pub enum KindFacts {
    Worktree(WorktreeFacts),
    BuildDir(BuildDirFacts),
    Cache(CacheFacts),
    Docker(DockerFacts),
    Steam(SteamFacts),
    Btrfs(BtrfsFacts),
}

impl KindFacts {
    pub fn kind(&self) -> Kind {
        match self {
            KindFacts::Worktree(_) => Kind::Worktree,
            KindFacts::BuildDir(b) => match b.flavor {
                BuildDirFlavor::NodeModules => Kind::NodeModules,
                BuildDirFlavor::CargoTarget => Kind::CargoTarget,
            },
            KindFacts::Cache(_) => Kind::Cache,
            KindFacts::Docker(
                DockerFacts::DanglingImage { .. } | DockerFacts::UnusedImage { .. },
            ) => Kind::DockerImage,
            KindFacts::Docker(DockerFacts::BuildCache) => Kind::DockerBuildCache,
            KindFacts::Docker(DockerFacts::Volume { .. }) => Kind::DockerVolume,
            KindFacts::Steam(SteamFacts::Prefix { .. }) => Kind::SteamPrefix,
            KindFacts::Steam(SteamFacts::ShaderCache { .. }) => Kind::SteamShaderCache,
            KindFacts::Steam(SteamFacts::Remnant { .. }) => Kind::SteamRemnant,
            KindFacts::Btrfs(BtrfsFacts::Balance { .. }) => Kind::BtrfsBalance,
            KindFacts::Btrfs(BtrfsFacts::Trim) => Kind::BtrfsTrim,
        }
    }
}

/// One discovered cleanable thing.
///
/// The `selector` and the facts are private: the constructor derives the selector with `selector_of`, so a
/// candidate cannot disagree with its facts, and `Kind::Cache` cannot be paired with docker facts.
///
/// ```compile_fail
/// use disk_cleanup_core::{Candidate, SelectorId};
/// fn tamper(c: &mut Candidate) {
///     c.selector = SelectorId::Steam;
/// }
/// ```
///
/// ```compile_fail
/// use disk_cleanup_core::{Candidate, Kind};
/// fn tamper(c: &mut Candidate) {
///     c.kind = Kind::Cache;
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "CandidateWire", try_from = "CandidateWire")]
pub struct Candidate {
    id: CandidateId,
    locator: Locator,
    est_bytes: AllocatedBytes,
    newest_mtime: Option<i64>,
    ident: Identity,
    facts: KindFacts,
    selector: SelectorId,
}

/// The wire schema of [`Candidate`].
pub const CANDIDATE_SCHEMA: u32 = 1;

impl Candidate {
    pub fn new(
        locator: Locator,
        facts: KindFacts,
        est_bytes: AllocatedBytes,
        newest_mtime: Option<i64>,
        ident: Identity,
    ) -> Candidate {
        let selector = selector_of(&facts);
        let id = CandidateId::new(facts.kind(), &locator);
        Candidate {
            id,
            locator,
            est_bytes,
            newest_mtime,
            ident,
            facts,
            selector,
        }
    }

    pub fn id(&self) -> CandidateId {
        self.id
    }
    pub fn locator(&self) -> &Locator {
        &self.locator
    }
    pub fn kind(&self) -> Kind {
        self.facts.kind()
    }
    pub fn selector(&self) -> SelectorId {
        self.selector
    }
    pub fn facts(&self) -> &KindFacts {
        &self.facts
    }
    /// Size at discovery (`allocated` bytes).
    pub fn est_bytes(&self) -> AllocatedBytes {
        self.est_bytes
    }
    /// Newest mtime inside, as Unix seconds.
    pub fn newest_mtime(&self) -> Option<i64> {
        self.newest_mtime
    }
    pub fn ident(&self) -> Identity {
        self.ident
    }
}

#[derive(Serialize, Deserialize)]
struct CandidateWire {
    schema: u32,
    id: CandidateId,
    kind: Kind,
    selector: SelectorId,
    locator: Locator,
    est_bytes: u64,
    newest_mtime: Option<i64>,
    #[serde(with = "IdentityDef")]
    ident: Identity,
    facts: KindFacts,
}

impl From<Candidate> for CandidateWire {
    fn from(c: Candidate) -> CandidateWire {
        CandidateWire {
            schema: CANDIDATE_SCHEMA,
            id: c.id,
            kind: c.facts.kind(),
            selector: c.selector,
            locator: c.locator,
            est_bytes: c.est_bytes.0,
            newest_mtime: c.newest_mtime,
            ident: c.ident,
            facts: c.facts,
        }
    }
}

impl TryFrom<CandidateWire> for Candidate {
    type Error = String;

    fn try_from(w: CandidateWire) -> Result<Candidate, String> {
        if w.schema != CANDIDATE_SCHEMA {
            return Err(format!("unknown candidate schema {}", w.schema));
        }
        if w.kind != w.facts.kind() {
            return Err("candidate kind disagrees with its facts".into());
        }
        if w.selector != selector_of(&w.facts) {
            return Err("candidate selector disagrees with its facts".into());
        }
        Ok(Candidate {
            id: w.id,
            locator: w.locator,
            est_bytes: AllocatedBytes(w.est_bytes),
            newest_mtime: w.newest_mtime,
            ident: w.ident,
            facts: w.facts,
            selector: w.selector,
        })
    }
}

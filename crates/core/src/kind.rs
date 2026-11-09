//! `Kind`, `Risk`, the `--kinds` ids (`SelectorId`) and the rule that derives one from the other.

use serde::{Deserialize, Serialize};

use crate::candidate::{DockerFacts, KindFacts};

/// Closed list of cleanable families. The exact kind appears in `--json` and the audit log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Cache,
    Worktree,
    NodeModules,
    CargoTarget,
    DockerImage,
    DockerBuildCache,
    DockerVolume,
    SteamPrefix,
    SteamShaderCache,
    SteamRemnant,
    BtrfsBalance,
    BtrfsTrim,
}

impl Kind {
    pub const ALL: [Kind; 12] = [
        Kind::Cache,
        Kind::Worktree,
        Kind::NodeModules,
        Kind::CargoTarget,
        Kind::DockerImage,
        Kind::DockerBuildCache,
        Kind::DockerVolume,
        Kind::SteamPrefix,
        Kind::SteamShaderCache,
        Kind::SteamRemnant,
        Kind::BtrfsBalance,
        Kind::BtrfsTrim,
    ];

    /// The kebab-case id used in JSON and the audit log.
    pub fn id(self) -> &'static str {
        match self {
            Kind::Cache => "cache",
            Kind::Worktree => "worktree",
            Kind::NodeModules => "node-modules",
            Kind::CargoTarget => "cargo-target",
            Kind::DockerImage => "docker-image",
            Kind::DockerBuildCache => "docker-build-cache",
            Kind::DockerVolume => "docker-volume",
            Kind::SteamPrefix => "steam-prefix",
            Kind::SteamShaderCache => "steam-shader-cache",
            Kind::SteamRemnant => "steam-remnant",
            Kind::BtrfsBalance => "btrfs-balance",
            Kind::BtrfsTrim => "btrfs-trim",
        }
    }

    /// The tier table of the plan's Glossary.
    pub fn risk(self) -> Risk {
        match self {
            Kind::Cache => Risk::Safe,
            Kind::Worktree
            | Kind::DockerImage
            | Kind::DockerBuildCache
            | Kind::SteamRemnant
            | Kind::BtrfsBalance
            | Kind::BtrfsTrim => Risk::Guarded,
            Kind::NodeModules
            | Kind::CargoTarget
            | Kind::DockerVolume
            | Kind::SteamPrefix
            | Kind::SteamShaderCache => Risk::Destructive,
        }
    }
}

/// Decides whether a kind is in the default `--apply` set. JSON field `tier`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Safe,
    Guarded,
    Destructive,
}

/// The ids `--kinds` accepts. Several selectors can sit behind one [`Kind`] (`caches` and `cache-apt` behind
/// `Cache`, `docker` and `docker-images` behind `DockerImage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SelectorId {
    Caches,
    CacheApt,
    Worktrees,
    BuildDirs,
    Docker,
    DockerImages,
    DockerVolumes,
    Steam,
    Btrfs,
    BtrfsTrim,
}

impl SelectorId {
    pub const ALL: [SelectorId; 10] = [
        SelectorId::Caches,
        SelectorId::CacheApt,
        SelectorId::Worktrees,
        SelectorId::BuildDirs,
        SelectorId::Docker,
        SelectorId::DockerImages,
        SelectorId::DockerVolumes,
        SelectorId::Steam,
        SelectorId::Btrfs,
        SelectorId::BtrfsTrim,
    ];

    /// The id as typed after `--kinds`.
    pub fn id(self) -> &'static str {
        match self {
            SelectorId::Caches => "caches",
            SelectorId::CacheApt => "cache-apt",
            SelectorId::Worktrees => "worktrees",
            SelectorId::BuildDirs => "build-dirs",
            SelectorId::Docker => "docker",
            SelectorId::DockerImages => "docker-images",
            SelectorId::DockerVolumes => "docker-volumes",
            SelectorId::Steam => "steam",
            SelectorId::Btrfs => "btrfs",
            SelectorId::BtrfsTrim => "btrfs-trim",
        }
    }

    /// The kinds an item selected by this id can have.
    pub fn kinds(self) -> &'static [Kind] {
        match self {
            SelectorId::Caches | SelectorId::CacheApt => &[Kind::Cache],
            SelectorId::Worktrees => &[Kind::Worktree],
            SelectorId::BuildDirs => &[Kind::NodeModules, Kind::CargoTarget],
            SelectorId::Docker => &[Kind::DockerImage, Kind::DockerBuildCache],
            SelectorId::DockerImages => &[Kind::DockerImage],
            SelectorId::DockerVolumes => &[Kind::DockerVolume],
            SelectorId::Steam => &[
                Kind::SteamPrefix,
                Kind::SteamShaderCache,
                Kind::SteamRemnant,
            ],
            SelectorId::Btrfs => &[Kind::BtrfsBalance],
            SelectorId::BtrfsTrim => &[Kind::BtrfsTrim],
        }
    }

    /// The highest tier among the kinds behind this id (what `kinds` shows).
    pub fn tier(self) -> Risk {
        self.kinds()
            .iter()
            .map(|k| k.risk())
            .max()
            .unwrap_or(Risk::Safe)
    }

    /// Destructive selectors decide the plan-id gate: exactly `build-dirs`, `docker-volumes` and `steam`.
    pub fn is_destructive(self) -> bool {
        self.tier() == Risk::Destructive
    }
}

/// Derives the `--kinds` id from the facts, so a candidate cannot carry a selector that disagrees with them.
pub fn selector_of(facts: &KindFacts) -> SelectorId {
    match facts {
        KindFacts::Cache(c) => {
            if c.apt {
                SelectorId::CacheApt
            } else {
                SelectorId::Caches
            }
        }
        KindFacts::Worktree(_) => SelectorId::Worktrees,
        KindFacts::BuildDir(_) => SelectorId::BuildDirs,
        KindFacts::Docker(DockerFacts::DanglingImage { .. } | DockerFacts::BuildCache) => {
            SelectorId::Docker
        }
        KindFacts::Docker(DockerFacts::UnusedImage { .. }) => SelectorId::DockerImages,
        KindFacts::Docker(DockerFacts::Volume { .. }) => SelectorId::DockerVolumes,
        KindFacts::Steam(_) => SelectorId::Steam,
        KindFacts::Btrfs(crate::candidate::BtrfsFacts::Balance { .. }) => SelectorId::Btrfs,
        KindFacts::Btrfs(crate::candidate::BtrfsFacts::Trim) => SelectorId::BtrfsTrim,
    }
}

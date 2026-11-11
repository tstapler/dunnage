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
    /// Test-only (Story 2.3.4): a stale directory removed by the `fixture-destructive` target. The variant
    /// does not exist in a build without `test-support`, so a release binary cannot name it.
    #[cfg(feature = "test-support")]
    FixtureStale,
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
            #[cfg(feature = "test-support")]
            Kind::FixtureStale => "fixture-stale",
        }
    }

    /// The family id the KIND column shows (the `--kinds` id of the family the kind belongs to).
    pub fn family(self) -> &'static str {
        match self {
            Kind::Cache => "caches",
            Kind::Worktree => "worktrees",
            Kind::NodeModules | Kind::CargoTarget => "build-dirs",
            Kind::DockerImage | Kind::DockerBuildCache | Kind::DockerVolume => "docker",
            Kind::SteamPrefix | Kind::SteamShaderCache | Kind::SteamRemnant => "steam",
            Kind::BtrfsBalance | Kind::BtrfsTrim => "btrfs",
            #[cfg(feature = "test-support")]
            Kind::FixtureStale => "fixture-destructive",
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
            #[cfg(feature = "test-support")]
            Kind::FixtureStale => Risk::Destructive,
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
    /// Test-only (Story 2.3.4): drives the destructive flow of the Gate B end-to-end tests.
    #[cfg(feature = "test-support")]
    FixtureDestructive,
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

    /// The selectors that exist only in a `test-support` build; the registry lists them when
    /// `DC_TEST_ROOT` is set.
    #[cfg(feature = "test-support")]
    pub const TEST_ONLY: [SelectorId; 1] = [SelectorId::FixtureDestructive];
    #[cfg(not(feature = "test-support"))]
    pub const TEST_ONLY: [SelectorId; 0] = [];

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
            #[cfg(feature = "test-support")]
            SelectorId::FixtureDestructive => "fixture-destructive",
        }
    }

    /// What the `kinds` table says the selector touches.
    pub fn touches(self) -> &'static str {
        match self {
        SelectorId::Caches => "npm, go, gradle, pip, cargo, playwright",
        SelectorId::CacheApt => "/var/cache/apt",
        SelectorId::Worktrees => ".claude/worktrees/agent-*",
        SelectorId::BuildDirs => "node_modules, cargo target",
        SelectorId::Docker => "dangling images, build cache",
        SelectorId::DockerImages => "unused tagged images pulled from a registry (locally built ones are skipped, LocalOnlyImage)",
        SelectorId::DockerVolumes => "unreferenced volumes (--docker-volumes anonymous|named)",
        SelectorId::Steam => "orphan prefixes, shader caches, uninstalled-game remnants",
        SelectorId::Btrfs => "balance (usage ladder)",
        SelectorId::BtrfsTrim => "fstrim",
        #[cfg(feature = "test-support")]
        SelectorId::FixtureDestructive => "directories named stale-* below --root (test only)",
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
            #[cfg(feature = "test-support")]
            SelectorId::FixtureDestructive => &[Kind::FixtureStale],
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
        #[cfg(feature = "test-support")]
        KindFacts::Fixture(_) => SelectorId::FixtureDestructive,
    }
}

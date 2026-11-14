//! Which rows the cleaner would list (Story 6.4.1): a pure tri-state hint with no I/O (G-3).
//!
//! `Yes` means the engine lists the path before its guards, `Shape` means right place and name but the
//! explorer lacks data (git, the ancestors above the scan root, Steam), `No` carries the reason. Build
//! directories are decided by the one `classify_dir` of the scan crate; the root test reads the cleaner's own
//! [`RootSet`], never the explore PATH.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use dunnage_scan::classify::{classify_dir, ParentFacts};
use dunnage_scan::scan::BuildKind;

use super::msg::{ClassifyFacts, Hint, NoReason, Tier};
use super::rows::Badge;
use super::tree::{ExploreTree, Inside, NodeId};
use crate::candidate::CargoPart;
use crate::delete_types::AllowedTarget;
use crate::ids::AppId;
use crate::kind::{Kind, Risk, SelectorId};
use crate::roots::{lexically_clean, RootSet};

impl Tier {
    pub fn of(risk: Risk) -> Tier {
        match risk {
            Risk::Safe => Tier::Safe,
            Risk::Guarded => Tier::Guarded,
            Risk::Destructive => Tier::Destructive,
        }
    }
}

impl Hint {
    /// `+` for a candidate the engine lists, `?` for a possible one, nothing otherwise.
    pub fn badge(self) -> Badge {
        match self {
            Hint::Yes { .. } => Badge::Candidate,
            Hint::Shape { .. } => Badge::Possible,
            Hint::No(_) => Badge::None,
        }
    }
}

fn selector_of_kind(kind: Kind) -> Option<SelectorId> {
    Some(match kind {
        Kind::Cache => SelectorId::Caches,
        Kind::Worktree => SelectorId::Worktrees,
        Kind::CargoHome => SelectorId::CargoHome,
        Kind::NodeModules | Kind::CargoTarget | Kind::CargoArtifact => SelectorId::BuildDirs,
        Kind::SteamPrefix | Kind::SteamShaderCache | Kind::SteamRemnant => SelectorId::Steam,
        _ => return None,
    })
}

fn ancestor_name(path: &Path, up: usize) -> Option<&OsStr> {
    path.ancestors().nth(up)?.file_name()
}

fn is_app_id(name: &OsStr) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.iter().all(u8::is_ascii_digit)
}

/// The kind whose name and place `path` has, without asking whether the cleaner may touch that place.
/// Lexical: no root, no filesystem. Docker and btrfs have no path, so they never match.
pub fn shape_of(path: &Path, parent: ParentFacts, facts: &ClassifyFacts) -> Option<Kind> {
    if facts.cache_paths.iter().any(|p| p == path) {
        return Some(Kind::Cache);
    }
    let name = path.file_name()?;
    if let Some(b) = classify_dir(name, parent) {
        return Some(match b {
            BuildKind::NodeModules => Kind::NodeModules,
            BuildKind::CargoTarget => Kind::CargoTarget,
        });
    }
    let dir = ancestor_name(path, 1);
    // `registry/src/<index>/<crate>` and `git/checkouts/<repo>/<rev>`: the place is decided by `under_allowed_root`
    if (ancestor_name(path, 2) == Some(OsStr::new("src"))
        && ancestor_name(path, 3) == Some(OsStr::new("registry")))
        || (ancestor_name(path, 2) == Some(OsStr::new("checkouts"))
            && ancestor_name(path, 3) == Some(OsStr::new("git")))
    {
        return Some(Kind::CargoHome);
    }
    if name
        .as_bytes()
        .strip_prefix(b"agent-")
        .is_some_and(|r| !r.is_empty())
        && dir == Some(OsStr::new("worktrees"))
        && ancestor_name(path, 2) == Some(OsStr::new(".claude"))
    {
        return Some(Kind::Worktree);
    }
    if ancestor_name(path, 2) == Some(OsStr::new("steamapps")) {
        return match dir?.as_bytes() {
            b"compatdata" if is_app_id(name) => Some(Kind::SteamPrefix),
            b"shadercache" if is_app_id(name) => Some(Kind::SteamShaderCache),
            b"common" => Some(Kind::SteamRemnant),
            _ => None,
        };
    }
    None
}

/// Whether `path` lies where a delete of `kind` may land, by the cleaner's roots, ignoring the name
/// pattern (that is the shape's job). A scan root itself is not below it.
fn under_allowed_root(roots: &RootSet, kind: Kind, path: &Path) -> bool {
    if !lexically_clean(path) {
        return false;
    }
    if kind == Kind::CargoHome {
        // exactly two levels below `registry/src` or `git/checkouts`: the engine's own test
        return roots.admits(kind, path);
    }
    roots.allowed(kind).iter().any(|t| match t {
        AllowedTarget::Exact(p) => p == path,
        AllowedTarget::BelowScanRoot(scope) => scope.path() == path,
        AllowedTarget::Within { root, .. } => path.strip_prefix(root).is_ok_and(|rel| {
            let depth = rel.components().count();
            match kind {
                Kind::SteamPrefix | Kind::SteamShaderCache | Kind::SteamRemnant => depth == 1,
                _ => depth >= 1,
            }
        }),
    })
}

enum SteamVerdict {
    Unknown,
    NotAnItem,
    Installed,
    Orphan,
}

/// Only an orphan prefix, an orphan shader cache or an uninstalled remnant is listed, and only when every
/// library was read, so an absent or partial answer stays unknown.
fn steam_verdict(kind: Kind, name: &OsStr, facts: &ClassifyFacts) -> SteamVerdict {
    let Some(libs) = &facts.steam else {
        return SteamVerdict::Unknown;
    };
    let installed_ids = || libs.iter().flat_map(|l| l.installed.iter().copied());
    // An unmounted library, or no installed app at all, means the manifests were not read (the engine's
    // `LibraryUnmounted` gate): nothing may be called an orphan.
    if libs.iter().any(|l| !l.mounted) || installed_ids().next().is_none() {
        return SteamVerdict::Unknown;
    }
    if kind == Kind::SteamRemnant {
        let Some(n) = name.to_str() else {
            return SteamVerdict::NotAnItem;
        };
        let n = n.to_lowercase();
        let named = libs
            .iter()
            .flat_map(|l| l.install_dirs.iter())
            .any(|d| d.to_str().is_some_and(|d| d.to_lowercase() == n));
        return if named {
            SteamVerdict::Installed
        } else {
            SteamVerdict::Orphan
        };
    }
    let Some(id) = name.to_str().and_then(|s| s.parse::<u64>().ok()).map(AppId) else {
        return SteamVerdict::NotAnItem;
    };
    if id.is_non_steam_shortcut() {
        return SteamVerdict::NotAnItem;
    }
    if installed_ids().any(|a| a == id) {
        SteamVerdict::Installed
    } else {
        SteamVerdict::Orphan
    }
}

/// `parent` holds the marker flags of `path`'s parent directory, `inside` is
/// [`ExploreTree::inside_special`] of the row. `roots` are the cleaner's (config, flag or defaults), never
/// the explore PATH.
///
/// Order: no shape is `No(NotShape)`; a shape outside the kind's roots is `No(OutsideRoot)` (the outer
/// directory would be outside too, so advising it would be wrong); inside a build directory or `.git` is
/// `No(InsideBuildOrGit)`; then the kind decides `Yes` or `Shape`.
pub fn kind_hint(
    path: &Path,
    parent: ParentFacts,
    inside: Inside,
    roots: &RootSet,
    facts: &ClassifyFacts,
) -> Hint {
    let Some(kind) = shape_of(path, parent, facts) else {
        return Hint::No(NoReason::NotShape);
    };
    let Some(selector) = selector_of_kind(kind) else {
        return Hint::No(NoReason::NotShape);
    };
    if !under_allowed_root(roots, kind, path) {
        return Hint::No(NoReason::OutsideRoot);
    }
    if inside == Inside::Yes {
        return Hint::No(NoReason::InsideBuildOrGit);
    }
    let tier = Tier::of(kind.risk());
    let yes = Hint::Yes { selector, tier };
    let shape = Hint::Shape { selector, tier };
    match kind {
        Kind::Cache => yes,
        Kind::NodeModules | Kind::CargoTarget => {
            if inside == Inside::Unknown {
                shape
            } else {
                yes
            }
        }
        // The engine lists worktrees through `git worktree list`, which the explorer does not run.
        Kind::Worktree => shape,
        // The engine lists a cargo-home entry only when it is old enough, which the explorer does not know.
        Kind::CargoHome => shape,
        _ => match steam_verdict(kind, path.file_name().unwrap_or_default(), facts) {
            SteamVerdict::Unknown => shape,
            SteamVerdict::NotAnItem => Hint::No(NoReason::NotShape),
            SteamVerdict::Installed => Hint::No(NoReason::InstalledSteamGame),
            SteamVerdict::Orphan => yes,
        },
    }
}

/// The cargo target a part-named node sits in: `<target>/<profile>/<part>` or `<target>/<triple>/<profile>/<part>`,
/// where `<target>` is a build dir by [`classify_dir`] with its own parent's marker flags.
fn cargo_target_of(tree: &ExploreTree, id: NodeId) -> Option<NodeId> {
    let name = tree.path_of(id).file_name()?.to_str()?.to_string();
    CargoPart::from_dir_name(&name)?;
    let profile = tree.parent(id)?;
    let above = tree.parent(profile)?;
    [above, tree.parent(above)?].into_iter().find(|n| {
        let Some(p) = tree.parent(*n) else {
            return false;
        };
        let facts = ParentFacts {
            has_package_json: tree.has_package_json(p),
            has_cargo_toml: tree.has_cargo_toml(p),
        };
        tree.path_of(*n)
            .file_name()
            .is_some_and(|nm| classify_dir(nm, facts) == Some(BuildKind::CargoTarget))
    })
}

/// A part of a cargo target is at most a possible `build-dirs` item: the engine offers it only when its
/// profile has been idle, which the explorer cannot tell. `None` when the node is not part-shaped, so the
/// ordinary rules decide.
fn cargo_part_hint(tree: &ExploreTree, id: NodeId, path: &Path, roots: &RootSet) -> Option<Hint> {
    let target = cargo_target_of(tree, id)?;
    if !roots.admits(Kind::CargoArtifact, path) {
        return Some(Hint::No(NoReason::OutsideRoot));
    }
    if tree.inside_special(target) == Inside::Yes {
        return Some(Hint::No(NoReason::InsideBuildOrGit));
    }
    Some(Hint::Shape {
        selector: SelectorId::BuildDirs,
        tier: Tier::of(Kind::CargoArtifact.risk()),
    })
}

/// The hint of one tree node: the parent's marker flags and the ancestor context come from the tree.
///
/// A tree root has no parent in the tree, so its marker files are unknown: a root named like a build
/// directory is `Shape` (the engine may list it), never `No(NotShape)` and never `Yes`.
pub fn hint_of_node(
    tree: &ExploreTree,
    id: NodeId,
    roots: &RootSet,
    facts: &ClassifyFacts,
) -> Hint {
    let path = tree.path_of(id);
    if let Some(h) = cargo_part_hint(tree, id, &path, roots) {
        return h;
    }
    let inside = tree.inside_special(id);
    let Some(p) = tree.parent(id) else {
        let assumed = ParentFacts {
            has_package_json: true,
            has_cargo_toml: true,
        };
        return match kind_hint(&path, assumed, inside, roots, facts) {
            Hint::Yes {
                selector: SelectorId::BuildDirs,
                tier,
            } => Hint::Shape {
                selector: SelectorId::BuildDirs,
                tier,
            },
            other => other,
        };
    };
    let parent = ParentFacts {
        has_package_json: tree.has_package_json(p),
        has_cargo_toml: tree.has_cargo_toml(p),
    };
    kind_hint(&path, parent, inside, roots, facts)
}

/// The shape of one tree node, for [`kind_line`]'s `OutsideRoot` text: [`shape_of`] with the parent's marker
/// flags read from the tree (`None` for a root, whose parent is unknown).
pub fn shape_of_node(tree: &ExploreTree, id: NodeId, facts: &ClassifyFacts) -> Option<Kind> {
    let p = tree.parent(id)?;
    let parent = ParentFacts {
        has_package_json: tree.has_package_json(p),
        has_cargo_toml: tree.has_cargo_toml(p),
    };
    shape_of(&tree.path_of(id), parent, facts)
}

fn tier_words(t: Tier) -> &'static str {
    match t {
        Tier::Safe => "safe tier",
        Tier::Guarded => "guarded",
        Tier::Destructive => "destructive tier",
    }
}

fn family(s: SelectorId) -> &'static str {
    match s {
        SelectorId::Caches => "caches",
        SelectorId::Worktrees => "worktrees",
        SelectorId::BuildDirs => "build-dirs",
        SelectorId::Steam => "steam",
        other => other.id(),
    }
}

/// The info pane's Kind line. `shape` is [`shape_of`] of the row, needed only to name the family of a
/// `No(OutsideRoot)` row.
pub fn kind_line(hint: Hint, shape: Option<Kind>) -> String {
    match hint {
        Hint::Yes { selector, tier } => format!(
            "{} ({}): the cleaner lists it",
            family(selector),
            tier_words(tier)
        ),
        Hint::Shape { selector, tier } => format!(
            "{} ({}): possible candidate, p checks",
            family(selector),
            tier_words(tier)
        ),
        Hint::No(NoReason::NotShape) => "not a cleaner item: deleted here as a plain path".into(),
        Hint::No(NoReason::OutsideRoot) => format!(
            "{} shape, outside the cleaner's folders: not markable",
            shape.map_or("cleaner", Kind::family)
        ),
        Hint::No(NoReason::InstalledSteamGame) => {
            "not a cleaner item: an installed Steam game, deleted here as a plain path".into()
        }
        Hint::No(NoReason::InsideBuildOrGit) => "not a separate cleaner item: inside a build \
            directory or .git that the cleaner lists as one item"
            .into(),
    }
}

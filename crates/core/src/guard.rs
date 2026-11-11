//! decide_path_common and the guard framework; the only caller of Allowed::new (Story 1.1.3).

use std::path::PathBuf;

use crate::allowed::Allowed;
use crate::ids::{Identity, RootDev};
use crate::kind::Kind;
use crate::outcome::SkipReason;
use crate::roots::{lexically_clean, RootSet};

/// Fresh, impure probe results about one path, fed to the pure `decide`. The device is `ident.dev` from
/// `lstat`, never read from the mount table (subvolumes share a `0:26` mountinfo field but differ in `st_dev`).
#[derive(Debug, Clone)]
pub struct Facts {
    pub kind: Kind,
    pub path: PathBuf,
    pub ident: Identity,
    pub is_symlink: bool,
    pub root_dev: RootDev,
    pub is_mount_point: bool,
}

#[derive(Debug)]
pub enum GuardDecision {
    Allow(Allowed),
    Skip(SkipReason),
}

/// A `.git` component anywhere in the path: no kind may delete inside or at one. Shared by the explorer's mark
/// rule M2.
pub(crate) fn has_git_component(path: &std::path::Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".git")
}

/// A scan root itself or a path with a `.git` component: refused by every kind, and by the explorer's rule M2
/// through this same function, so the screen and the engine cannot disagree on it.
pub(crate) fn protected_position(path: &std::path::Path, scan_roots: &[PathBuf]) -> bool {
    scan_roots.iter().any(|r| r == path) || has_git_component(path)
}

/// The checks every path-based kind shares. A returned `Allow` is the only way to obtain an [`Allowed`].
/// The lexical test runs first: `starts_with(root)` is meaningless on an unclean string (`/h/code/../.ssh`).
pub fn decide_path_common(facts: &Facts, roots: &RootSet) -> GuardDecision {
    use GuardDecision::Skip;
    let path = facts.path.as_path();
    if !lexically_clean(path) {
        return Skip(SkipReason::OutsideRoot);
    }
    if facts.is_symlink {
        return Skip(SkipReason::Symlink);
    }
    if protected_position(path, roots.scan_roots()) {
        return Skip(SkipReason::ProtectedPath);
    }
    // `$HOME` or an ancestor under another spelling (a bind mount): the lexical test above cannot see it.
    if roots.in_home_chain(facts.ident) {
        return Skip(SkipReason::ProtectedPath);
    }
    if !facts.ident.on_dev(facts.root_dev) {
        return Skip(SkipReason::CrossDevice);
    }
    if facts.is_mount_point {
        return Skip(SkipReason::MountPoint);
    }
    if !roots.admits(facts.kind, path) {
        return Skip(SkipReason::OutsideRoot);
    }
    GuardDecision::Allow(Allowed::new(facts.path.clone(), facts.ident, facts.kind))
}

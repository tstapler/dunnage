//! Rule F: which paths `delete_tree` may ever remove. Pure and filesystem-free.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::delete_types::{AllowedTarget, DeleteError, DeletePolicy, NamePattern};

/// Absolute, and re-joining its normal components gives back exactly the input: no `..`, `.`, empty
/// component or trailing `/`.
fn is_clean_abs(p: &Path) -> bool {
    let mut comps = p.components();
    if comps.next() != Some(Component::RootDir) {
        return false;
    }
    let mut joined = PathBuf::from("/");
    for c in comps {
        match c {
            Component::Normal(n) => joined.push(n),
            _ => return false,
        }
    }
    joined.as_os_str() == p.as_os_str()
}

fn components_below(path: &Path, base: &Path) -> Option<usize> {
    path.strip_prefix(base).ok().map(|r| r.components().count())
}

/// `/`, `$HOME`, an ancestor of `$HOME` or a scan root: the first floor refusal of [`DeletePolicy::validate`].
fn is_structurally_protected(target: &Path, home: &Path, scan_roots: &[PathBuf]) -> bool {
    target == Path::new("/")
        || target == home
        || home.starts_with(target)
        || scan_roots.iter().any(|r| r == target)
}

/// Fewer than two components below `$HOME`: the last floor refusal of [`DeletePolicy::validate`].
fn too_shallow_below_home(target: &Path, home: &Path) -> bool {
    components_below(target, home).is_some_and(|d| d < 2)
}

/// The position-only part of rule F: true when `validate` would answer `ProtectedPath` for a clean absolute
/// `target` that its allowed target admits. The explorer's mark rule M2 calls this same function (Story
/// 6.6.1), so a row can be marked only if the engine's floor would let the delete through.
pub fn floor_protected(target: &Path, home: &Path, scan_roots: &[PathBuf]) -> bool {
    is_structurally_protected(target, home, scan_roots) || too_shallow_below_home(target, home)
}

fn matches_pattern(pattern: NamePattern, base: &OsStr) -> bool {
    let b = base.as_bytes();
    match pattern {
        NamePattern::NodeModules => b == b"node_modules",
        NamePattern::CargoTarget => b == b"target",
        NamePattern::AgentWorktree => b.starts_with(b"agent-") && b.len() > b"agent-".len(),
        NamePattern::SteamAppId => !b.is_empty() && b.iter().all(u8::is_ascii_digit),
        NamePattern::SteamInstallDir => !b.is_empty() && b[0] != b'.',
    }
}

impl DeletePolicy {
    /// Requires `target`, `home`, every scan root and the allowed root to be absolute and lexically clean,
    /// then applies the floor rule (ADR-003 decision 2, Story 2.1.2): `name` is `target`'s basename; `target`
    /// is not `/`, `$HOME`, an ancestor of `$HOME` or a scan root; it satisfies `allowed`; and below `$HOME`
    /// it has at least two components.
    ///
    /// Lexical, so sound only behind `open_parent`, whose handle carries a symlink-free path.
    #[allow(unused_variables)]
    pub fn validate(&self, name: &OsStr) -> Result<(), DeleteError> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes == b"." || bytes == b".." {
            return Err(DeleteError::ProtectedPath);
        }
        if bytes.contains(&b'/') {
            return Err(DeleteError::OutsideScope);
        }
        let allowed_clean = match &self.allowed {
            AllowedTarget::Exact(p) => is_clean_abs(p),
            AllowedTarget::Within { root, .. } => is_clean_abs(root),
        };
        if !(allowed_clean
            && is_clean_abs(&self.target)
            && is_clean_abs(&self.home)
            && self.scan_roots.iter().all(|r| is_clean_abs(r)))
        {
            return Err(DeleteError::OutsideScope);
        }
        if self.target.file_name() != Some(name) {
            return Err(DeleteError::OutsideScope);
        }

        let target = self.target.as_path();
        if is_structurally_protected(target, &self.home, &self.scan_roots) {
            return Err(DeleteError::ProtectedPath);
        }

        let permitted = match &self.allowed {
            AllowedTarget::Exact(p) => target == p,
            AllowedTarget::Within {
                root,
                name: pattern,
            } => {
                let depth = components_below(target, root);
                let no_git = target
                    .strip_prefix(root)
                    .is_ok_and(|rel| rel.components().all(|c| c.as_os_str() != ".git"));
                let shape = match pattern {
                    NamePattern::SteamInstallDir => depth == Some(1),
                    _ => depth.is_some_and(|d| d >= 1),
                };
                shape && no_git && matches_pattern(*pattern, name)
            }
        };
        if !permitted {
            return Err(DeleteError::OutsideScope);
        }

        if too_shallow_below_home(target, &self.home) {
            return Err(DeleteError::ProtectedPath);
        }
        Ok(())
    }
}

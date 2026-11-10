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
        if target == Path::new("/")
            || target == self.home
            || self.home.starts_with(target)
            || self.scan_roots.iter().any(|r| r == target)
        {
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

        if components_below(target, &self.home).is_some_and(|d| d < 2) {
            return Err(DeleteError::ProtectedPath);
        }
        Ok(())
    }
}

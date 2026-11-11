//! Rule F: which paths `delete_tree` may ever remove. Pure and filesystem-free.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::allowed::{Allowed, ExplorerScope, ExplorerScopeWrongKind};
use crate::delete_types::FileKind;
use crate::delete_types::{AllowedTarget, DeleteError, DeletePolicy, NamePattern};
use crate::ids::{Identity, RootDev};
use crate::plan::ItemIdentity;
use crate::roots::RootSet;

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
    /// False when this policy was made from a witness (an explorer delete) that saw another identity than
    /// `expect`: `delete_tree` refuses then, so a fresh identity cannot stand in for the planned one (review 2,
    /// N4). Other policies are not bound to a witness identity.
    pub fn witness_matches(&self, expect: Identity) -> bool {
        match &self.allowed {
            AllowedTarget::BelowScanRoot(scope) => scope.ident() == expect,
            AllowedTarget::Exact(_) | AllowedTarget::Within { .. } => true,
        }
    }

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
            AllowedTarget::BelowScanRoot(scope) => is_clean_abs(scope.path()),
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
        // `$HOME` under another spelling (bind mount, alias): the lexical floor cannot see it.
        if let AllowedTarget::BelowScanRoot(scope) = &self.allowed {
            if scope.ident() == self.home_id {
                return Err(DeleteError::ProtectedPath);
            }
        }

        let permitted = match &self.allowed {
            AllowedTarget::Exact(p) => target == p,
            AllowedTarget::BelowScanRoot(scope) => {
                target == scope.path()
                    && self
                        .scan_roots
                        .iter()
                        .any(|r| components_below(target, r).is_some_and(|d| d >= 1))
                    && !target.components().any(|c| c.as_os_str() == ".git")
            }
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

/// Why [`DeletePolicy::for_allowed_roots`] refused to build a policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForAllowedError {
    /// The witness is not of kind `explorer-selection`.
    WrongKind,
    /// The plan recorded something other than a directory for this path.
    NotADirectory,
    /// The identity the guard just saw is not the identity the plan recorded: the path changed since the
    /// plan (a new inode, another device). Printed as `SKIP changed since the plan`.
    ChangedSincePlan,
    /// The witness names `$HOME` or an ancestor of it by identity, or is itself a scan root (by path or by
    /// identity): a target must be strictly below a scan root.
    ProtectedPath,
}

impl From<ExplorerScopeWrongKind> for ForAllowedError {
    fn from(_: ExplorerScopeWrongKind) -> ForAllowedError {
        ForAllowedError::WrongKind
    }
}

impl DeletePolicy {
    /// The way to build the policy of an explorer delete (Task 6.6.3f, review 2 N4): from the [`Allowed`]
    /// witness that `decide_path_common` minted for kind `explorer-selection`, with `home`, `home_id` and the
    /// scan roots read from the [`RootSet`] (a caller cannot pass `scan_roots = ["/"]` with a made-up home),
    /// and tied to the plan: `planned` is the [`ItemIdentity`] stored when the owner reviewed the plan, and
    /// the policy exists only if it equals what the guard saw just now. `delete_tree` then compares its
    /// `expect` with the same witness, so a fresh identity cannot stand in for the planned one.
    ///
    /// `root_mnt` is not optional: a root whose mount id could not be read cannot be deleted below (review
    /// C9). No sibling marker is required (the owner may mark any directory); depth and `.git` are checked
    /// by [`DeletePolicy::validate`]. `$HOME`, its ancestors by identity, and every scan root (by path or identity) are refused here too.
    pub fn for_allowed_roots(
        allowed: &Allowed,
        root_dev: RootDev,
        root_mnt: u64,
        roots: &RootSet,
        planned: ItemIdentity,
    ) -> Result<DeletePolicy, ForAllowedError> {
        let scope = ExplorerScope::from_allowed(allowed)?;
        if planned.kind != FileKind::Dir {
            return Err(ForAllowedError::NotADirectory);
        }
        let seen = scope.ident();
        if (planned.dev, planned.ino) != (seen.dev, seen.ino) {
            return Err(ForAllowedError::ChangedSincePlan);
        }
        if roots.in_home_chain(seen)
            || roots.is_scan_root(scope.path())
            || roots.root_identities().iter().any(|r| r.ident == seen)
        {
            return Err(ForAllowedError::ProtectedPath);
        }
        Ok(DeletePolicy {
            root_dev,
            root_mnt: Some(root_mnt),
            home: roots.home().to_path_buf(),
            home_id: roots.home_id(),
            scan_roots: roots.scan_roots().to_vec(),
            target: scope.path().to_path_buf(),
            allowed: AllowedTarget::BelowScanRoot(scope),
            require_sibling_any: Vec::new(),
            max_depth: 256,
            restore_owner_write: true,
        })
    }

    /// The earlier constructor, kept working until the independent test author moves the pinned suites to
    /// [`DeletePolicy::for_allowed_roots`] (ADR-003 process requirement 2). It trusts the loose `home`,
    /// `home_id` and `scan_roots` and ties nothing to a plan: production code must call `for_allowed_roots`.
    pub fn for_allowed(
        allowed: &Allowed,
        root_dev: RootDev,
        root_mnt: u64,
        home: PathBuf,
        home_id: Identity,
        scan_roots: Vec<PathBuf>,
    ) -> Result<DeletePolicy, ExplorerScopeWrongKind> {
        let scope = ExplorerScope::from_allowed(allowed)?;
        Ok(DeletePolicy {
            root_dev,
            root_mnt: Some(root_mnt),
            home,
            home_id,
            scan_roots,
            target: scope.path().to_path_buf(),
            allowed: AllowedTarget::BelowScanRoot(scope),
            require_sibling_any: Vec::new(),
            max_depth: 256,
            restore_owner_write: true,
        })
    }

    /// Unit tests build the policy from a bare scope (a `.git` path has no `Allowed`).
    #[cfg(test)]
    fn for_scope_tests(scope: ExplorerScope, roots: Vec<PathBuf>) -> DeletePolicy {
        DeletePolicy {
            root_dev: RootDev(1),
            root_mnt: Some(7),
            home: "/home/u".into(),
            home_id: Identity { dev: 1, ino: 2 },
            scan_roots: roots,
            target: scope.path().to_path_buf(),
            allowed: AllowedTarget::BelowScanRoot(scope),
            require_sibling_any: Vec::new(),
            max_depth: 256,
            restore_owner_write: true,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, non_snake_case)]
mod explorer_tests {
    use super::*;

    fn policy(target: &str, scope: &str) -> DeletePolicy {
        let mut p = DeletePolicy::for_scope_tests(
            ExplorerScope::test_scope(scope),
            vec!["/home/u/code".into()],
        );
        p.target = target.into();
        p
    }

    fn check(target: &str) -> Result<(), DeleteError> {
        let p = policy(target, target);
        p.validate(Path::new(target).file_name().unwrap_or_default())
    }

    #[test]
    fn validate_should_accept_a_directory_two_levels_below_a_scan_root_When_the_scope_is_for_it() {
        assert_eq!(check("/home/u/code/app"), Ok(()));
        assert_eq!(check("/home/u/code/a/b/c"), Ok(()));
    }

    #[test]
    fn validate_should_refuse_a_dot_git_component_anywhere_in_the_path() {
        assert_eq!(
            check("/home/u/code/app/.git"),
            Err(DeleteError::OutsideScope)
        );
        assert_eq!(
            check("/home/u/code/.git/hooks"),
            Err(DeleteError::OutsideScope)
        );
        assert_eq!(check("/home/u/code/.gitx"), Ok(()));
    }

    #[test]
    fn validate_should_refuse_the_scan_root_home_an_ancestor_and_slash() {
        assert_eq!(check("/home/u/code"), Err(DeleteError::ProtectedPath));
        assert_eq!(check("/home/u"), Err(DeleteError::ProtectedPath));
        assert_eq!(check("/home"), Err(DeleteError::ProtectedPath));
        assert_eq!(check("/"), Err(DeleteError::ProtectedPath));
    }

    #[test]
    fn validate_should_refuse_a_path_outside_every_scan_root() {
        assert_eq!(check("/home/u/other/app"), Err(DeleteError::OutsideScope));
        assert_eq!(check("/home/u/codex/app"), Err(DeleteError::OutsideScope));
        assert_eq!(check("/etc/app"), Err(DeleteError::OutsideScope));
    }

    #[test]
    fn validate_should_refuse_a_target_the_witness_was_not_made_for() {
        let p = policy("/home/u/code/other", "/home/u/code/app");
        assert_eq!(p.validate("other".as_ref()), Err(DeleteError::OutsideScope));
        let p = policy("/home/u/code/app/.git", "/home/u/code/app");
        assert_eq!(p.validate(".git".as_ref()), Err(DeleteError::OutsideScope));
    }

    #[test]
    fn validate_should_refuse_an_unclean_witness_or_target() {
        for t in [
            "/home/u/code/../.ssh",
            "/home/u/code//app",
            "/home/u/code/app/",
            "code/app",
        ] {
            let p = policy(t, t);
            let name = Path::new(t.trim_end_matches('/')).file_name().unwrap();
            assert_eq!(p.validate(name), Err(DeleteError::OutsideScope), "{t}");
        }
    }

    #[test]
    fn validate_should_apply_the_two_components_below_home_floor_to_a_root_outside_home() {
        let mut p = DeletePolicy::for_scope_tests(
            ExplorerScope::test_scope("/home/u/code/app"),
            vec!["/home/u".into()],
        );
        p.target = "/home/u/app".into();
        p.allowed = AllowedTarget::BelowScanRoot(ExplorerScope::test_scope("/home/u/app"));
        assert_eq!(p.validate("app".as_ref()), Err(DeleteError::ProtectedPath));
    }

    #[test]
    fn for_explorer_should_carry_the_mount_id_and_no_sibling_requirement() {
        let p = policy("/home/u/code/app", "/home/u/code/app");
        assert_eq!(p.root_mnt, Some(7));
        assert!(p.require_sibling_any.is_empty());
    }
}

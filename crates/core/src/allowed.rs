//! Witnesses that a path just passed the common path checks. Derive policy: `Debug` only, no trait impls
//! (`scripts/check-token-sites.sh` enforces it); `Allowed::new` is called only from `guard.rs`.

use std::path::{Path, PathBuf};

use crate::ids::Identity;
use crate::kind::Kind;

/// Compile-time proof "this path passed `decide_path_common` just now". Another crate cannot fabricate one:
///
/// ```compile_fail
/// use disk_cleanup_core::{Allowed, Identity, Kind};
/// let _ = Allowed::new("/x".into(), Identity { dev: 1, ino: 2 }, Kind::Cache);
/// ```
///
/// ```compile_fail
/// use disk_cleanup_core::{Allowed, Identity, Kind};
/// let _ = Allowed { path: "/x".into(), ident: Identity { dev: 1, ino: 2 }, kind: Kind::Cache };
/// ```
#[derive(Debug)]
pub struct Allowed {
    path: PathBuf,
    ident: Identity,
    kind: Kind,
}

impl Allowed {
    pub(crate) fn new(path: PathBuf, ident: Identity, kind: Kind) -> Allowed {
        Allowed { path, ident, kind }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn ident(&self) -> Identity {
        self.ident
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }
}

/// `WorktreeRemoval::new` was given an `Allowed` that is not a worktree.
#[derive(Debug, PartialEq, Eq)]
pub struct WrongKind;

impl std::fmt::Display for WrongKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a worktree removal needs an Allowed of kind Worktree")
    }
}

impl std::error::Error for WrongKind {}

/// The one deletion outside `delete_tree` (Story 3.1.3). It can only be built from an [`Allowed`] of kind
/// `Worktree`; `CommandRunner::remove_worktree` builds the `git -C <main_repo> worktree remove <path>` argv
/// from it, in one place.
///
/// ```compile_fail
/// use disk_cleanup_core::WorktreeRemoval;
/// let _ = WorktreeRemoval { allowed: todo!(), main_repo: "/r".into() };
/// ```
#[derive(Debug)]
pub struct WorktreeRemoval {
    allowed: Allowed,
    main_repo: PathBuf,
}

impl WorktreeRemoval {
    pub fn new(allowed: Allowed, main_repo: PathBuf) -> Result<WorktreeRemoval, WrongKind> {
        if allowed.kind != Kind::Worktree {
            return Err(WrongKind);
        }
        Ok(WorktreeRemoval { allowed, main_repo })
    }

    pub fn path(&self) -> &Path {
        self.allowed.path()
    }

    pub fn main_repo(&self) -> &Path {
        &self.main_repo
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::unwrap_in_result)]
mod tests {
    use super::*;

    // Built by literal: `Allowed::new` has exactly one call site, in guard.rs (check-token-sites.sh).
    fn allowed(kind: Kind) -> Allowed {
        Allowed {
            path: PathBuf::from("/r/.claude/worktrees/agent-1"),
            ident: Identity { dev: 1, ino: 2 },
            kind,
        }
    }

    #[test]
    fn worktree_removal_should_accept_only_an_allowed_of_kind_worktree() {
        let ok = WorktreeRemoval::new(allowed(Kind::Worktree), "/r".into()).unwrap();
        assert_eq!(ok.path(), Path::new("/r/.claude/worktrees/agent-1"));
        assert_eq!(ok.main_repo(), Path::new("/r"));
        for k in Kind::ALL.into_iter().filter(|k| *k != Kind::Worktree) {
            assert_eq!(
                WorktreeRemoval::new(allowed(k), "/r".into()).unwrap_err(),
                WrongKind,
                "{k:?}"
            );
        }
    }
}

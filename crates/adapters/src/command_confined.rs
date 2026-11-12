//! `ConfinedCommandRunner`: the subprocess half of the test confinement (Story 2.1.6). `DC_TEST_ROOT` confines
//! `DirOps`, but `brew cleanup` or `go clean` act on the real tool state, so under the fixture a mutating
//! command runs only when it is a test shim under `<root>/bin` or `git` working inside the fixture.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::command::{spawn_with_deadline, RealCommandRunner};
use dunnage_core::ports::{classify, worktree_remove_argv, Class, Cmd, CmdOutput, CommandRunner};
use dunnage_core::{ApplyToken, DeleteError, WorktreeRemoval};

pub struct ConfinedCommandRunner {
    inner: RealCommandRunner,
    root: PathBuf,
    path: OsString,
}

impl ConfinedCommandRunner {
    /// `root` is the canonical, existing `DC_TEST_ROOT`; programs are resolved through this process's `PATH`.
    pub fn new(inner: RealCommandRunner, root: PathBuf) -> Self {
        Self::with_path(inner, root, std::env::var_os("PATH").unwrap_or_default())
    }

    /// As [`new`](Self::new) with an explicit search path. A mutating command is spawned by the absolute path
    /// this resolves to, so the file that was checked is the file that runs; `remove_worktree` still finds
    /// `git` through the process `PATH`, so an explicit path is for tests that do not call it.
    pub fn with_path(inner: RealCommandRunner, root: PathBuf, path: OsString) -> Self {
        ConfinedCommandRunner { inner, root, path }
    }

    fn inside(&self, p: &Path) -> bool {
        if !p.is_absolute() {
            return false;
        }
        match std::fs::canonicalize(p) {
            Ok(c) => c.starts_with(&self.root),
            // Not on disk yet: only a path with no `..` can be judged lexically.
            Err(_) => {
                !p.components().any(|c| c == std::path::Component::ParentDir)
                    && p.starts_with(&self.root)
            }
        }
    }

    /// The file a bare program name resolves to: the first executable on the search path, as `execvp` finds it.
    fn resolve(&self, program: &OsString) -> Option<PathBuf> {
        std::env::split_paths(&self.path)
            .map(|d| d.join(program))
            .find(|p| {
                p.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
    }

    fn is_shim(&self, program: &OsString) -> bool {
        let bin = self.root.join("bin");
        self.resolve(program)
            .and_then(|p| std::fs::canonicalize(p).ok())
            .is_some_and(|p| p.starts_with(bin))
    }

    /// `git` with its directory (`-C <repo>`, else the working directory) and every path-like argument
    /// inside the fixture.
    fn is_fixture_git(&self, program: &OsString, args: &[OsString]) -> bool {
        if program != "git" {
            return false;
        }
        let (dir, rest) = match args {
            [c, repo, tail @ ..] if c == "-C" => (PathBuf::from(repo), tail),
            _ => (std::env::current_dir().unwrap_or_default(), args),
        };
        self.inside(&dir)
            && rest.iter().all(|a| {
                let p = Path::new(a);
                !(p.is_absolute() || a.to_string_lossy().contains('/')) || self.inside(p)
            })
    }

    fn allowed(&self, program: &OsString, args: &[OsString]) -> bool {
        self.is_shim(program) || self.is_fixture_git(program, args)
    }
}

impl CommandRunner for ConfinedCommandRunner {
    fn exists(&self, program: &str) -> bool {
        self.inner.exists(program)
    }

    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, DeleteError> {
        self.inner.run(cmd)
    }

    fn run_mutating(&self, cmd: &Cmd, proof: &ApplyToken) -> Result<CmdOutput, DeleteError> {
        let _ = proof;
        if classify(cmd) != Some(Class::Mutating) || !self.allowed(&cmd.program, &cmd.args) {
            return Err(DeleteError::Refused);
        }
        let Some(file) = self.resolve(&cmd.program) else {
            return Err(DeleteError::Io(2));
        };
        spawn_with_deadline(&file.into_os_string(), &cmd.args, self.inner.timeout())
    }

    fn remove_worktree(
        &self,
        w: &WorktreeRemoval,
        proof: &ApplyToken,
    ) -> Result<CmdOutput, DeleteError> {
        if !self.is_fixture_git(&OsString::from("git"), &worktree_remove_argv(w)) {
            return Err(DeleteError::Refused);
        }
        self.inner.remove_worktree(w, proof)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
mod tests {
    use super::*;

    fn runner(root: &Path) -> ConfinedCommandRunner {
        ConfinedCommandRunner::with_path(
            RealCommandRunner::new(),
            std::fs::canonicalize(root).unwrap(),
            OsString::new(),
        )
    }

    fn argv(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn git_should_be_allowed_only_when_its_repo_and_every_path_argument_are_inside_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let (root, out) = (
            tmp.path().to_str().unwrap(),
            outside.path().to_str().unwrap(),
        );
        let r = runner(tmp.path());
        let git = OsString::from("git");
        let inside_repo = format!("{root}/repo");
        let outside_repo = format!("{out}/repo");
        assert!(r.is_fixture_git(
            &git,
            &argv(&[
                "-C",
                &inside_repo,
                "worktree",
                "remove",
                &format!("{root}/wt")
            ])
        ));
        assert!(!r.is_fixture_git(
            &git,
            &argv(&[
                "-C",
                &outside_repo,
                "worktree",
                "remove",
                &format!("{root}/wt")
            ])
        ));
        assert!(!r.is_fixture_git(
            &git,
            &argv(&[
                "-C",
                &inside_repo,
                "worktree",
                "remove",
                &format!("{out}/wt")
            ])
        ));
        assert!(!r.is_fixture_git(&git, &argv(&["-C", "relative", "status"])));
        assert!(!r.is_fixture_git(&OsString::from("brew"), &argv(&["-C", &inside_repo])));
    }

    #[test]
    fn git_should_be_refused_when_dotdot_escapes_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let r = runner(tmp.path());
        let esc = format!("{}/../escape", tmp.path().display());
        assert!(!r.is_fixture_git(&OsString::from("git"), &argv(&["-C", &esc, "status"])));
    }
}

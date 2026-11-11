//! Agent-worktree discovery (Story 3.1.1): one `git worktree list --porcelain -z` per main repo, never a
//! path glob. A candidate is a path git names, directly below `<main>/.claude/worktrees/` and called
//! `agent-*`; descendants of a worktree are never looked at (spike 0.2.6: the old glob reported 1,610
//! "worktrees" for 8 real ones).

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use disk_cleanup_core::par::{default_workers, par_map};
use disk_cleanup_core::ports::{Cmd, CommandRunner};
use disk_cleanup_core::Identity;

/// Directory below a main repo that holds Claude Code agent worktrees.
const AGENT_DIR: [&str; 2] = [".claude", "worktrees"];

/// One record of `git worktree list --porcelain -z`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    /// `Some(reason)` when locked; the reason is empty when git printed none.
    pub locked: Option<String>,
    pub prunable: bool,
}

/// Parses the NUL-separated porcelain format: fields end in NUL, records in an extra NUL. A record without
/// a `worktree` field is dropped; unknown fields are ignored.
pub fn parse_porcelain_z(out: &[u8]) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut cur: Option<WorktreeEntry> = None;
    for field in out.split(|b| *b == 0) {
        if field.is_empty() {
            entries.extend(cur.take());
            continue;
        }
        if let Some(p) = field.strip_prefix(b"worktree ") {
            entries.extend(cur.take());
            cur = Some(WorktreeEntry {
                path: PathBuf::from(OsString::from_vec(p.to_vec())),
                ..WorktreeEntry::default()
            });
            continue;
        }
        let Some(e) = cur.as_mut() else { continue };
        let text = String::from_utf8_lossy(field);
        let (key, value) = text.split_once(' ').unwrap_or((&text, ""));
        match key {
            "HEAD" => e.head = Some(value.to_string()),
            "branch" => e.branch = Some(value.to_string()),
            "detached" => e.detached = true,
            "bare" => e.bare = true,
            "locked" => e.locked = Some(value.to_string()),
            "prunable" => e.prunable = true,
            _ => {}
        }
    }
    entries.extend(cur);
    entries
}

/// A removal candidate: a worktree git names under `<main_repo>/.claude/worktrees/agent-*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSite {
    pub main_repo: PathBuf,
    pub entry: WorktreeEntry,
    /// `lstat` identity taken now; `None` when the path is gone (a prunable entry).
    pub ident: Option<Identity>,
}

#[derive(Debug, Default)]
pub struct WorktreeDiscovery {
    pub sites: Vec<WorktreeSite>,
    /// `warning:` lines, one per repo that could not be listed.
    pub warnings: Vec<String>,
}

/// True for `<main>/.claude/worktrees/agent-<x>` exactly: one component below the agent directory.
pub fn is_agent_worktree_path(main_repo: &Path, path: &Path) -> bool {
    let dir = AGENT_DIR
        .iter()
        .fold(main_repo.to_path_buf(), |p, c| p.join(c));
    path.parent() == Some(dir.as_path())
        && path.file_name().is_some_and(|n| {
            n.as_bytes()
                .strip_prefix(b"agent-")
                .is_some_and(|r| !r.is_empty())
        })
}

fn git(repo: &Path, args: &[&str]) -> Cmd {
    let mut argv: Vec<OsString> = vec!["-C".into(), repo.as_os_str().to_owned()];
    argv.extend(args.iter().map(OsString::from));
    Cmd::new("git", argv, false)
}

/// Lists the worktrees of every main repo, one `git worktree list --porcelain -z` each. A repo whose list
/// fails yields a `warning:` line and no candidates; nothing is guessed from a directory walk.
pub fn discover_worktrees(runner: &dyn CommandRunner, main_repos: &[PathBuf]) -> WorktreeDiscovery {
    let per_repo = par_map(main_repos, default_workers(), |repo| {
        list_repo(runner, repo)
    });
    let mut out = WorktreeDiscovery::default();
    for (sites, warning) in per_repo {
        out.sites.extend(sites);
        out.warnings.extend(warning);
    }
    out
}

fn list_repo(runner: &dyn CommandRunner, repo: &Path) -> (Vec<WorktreeSite>, Option<String>) {
    let listed = runner.run(&git(repo, &["worktree", "list", "--porcelain", "-z"]));
    let bytes = match listed {
        Ok(o) => o.stdout,
        Err(e) => {
            let w = format!(
                "warning: {}: git worktree list failed ({e:?}); its worktrees were not examined",
                repo.display()
            );
            return (Vec::new(), Some(w));
        }
    };
    let sites = parse_porcelain_z(&bytes)
        .into_iter()
        .filter(|entry| !entry.bare && is_agent_worktree_path(repo, &entry.path))
        .map(|entry| {
            let ident = std::fs::symlink_metadata(&entry.path)
                .ok()
                .map(|m| Identity::of(&m));
            WorktreeSite {
                main_repo: repo.to_path_buf(),
                entry,
                ident,
            }
        })
        .collect();
    (sites, None)
}

/// The main repo that owns `checkout`, from `git rev-parse --git-common-dir` (the common git directory is
/// `<main>/.git`; a relative answer is relative to `checkout`). `None` for a bare repo or a failure.
fn main_repo_of(runner: &dyn CommandRunner, checkout: &Path) -> Result<PathBuf, String> {
    let o = runner
        .run(&git(checkout, &["rev-parse", "--git-common-dir"]))
        .map_err(|e| format!("{e:?}"))?;
    let text = o.stdout_lossy();
    let line = text.lines().next().unwrap_or("").trim_end();
    if line.is_empty() {
        return Err("empty answer".into());
    }
    let common = checkout.join(line);
    let common = std::fs::canonicalize(&common).map_err(|e| e.to_string())?;
    if common.file_name().is_some_and(|n| n == ".git") {
        common
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "no parent".into())
    } else {
        Err(format!("{} is not a .git directory", common.display()))
    }
}

#[derive(Debug, Default)]
pub struct RepoCollection {
    /// Each main repo once, sorted.
    pub repos: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// Dedupes `seeds` (a checkout or a worktree of some repo: the scan's `Group.repo` values, or the parents
/// of `.git` directories) to main repos by `git rev-parse --git-common-dir`.
pub fn collect_repos(runner: &dyn CommandRunner, seeds: &[PathBuf]) -> RepoCollection {
    let mut set = BTreeSet::new();
    let mut warnings = Vec::new();
    let mains = par_map(seeds, default_workers(), |seed| main_repo_of(runner, seed));
    for (seed, main) in seeds.iter().zip(mains) {
        match main {
            Ok(main) => {
                set.insert(main);
            }
            Err(why) => warnings.push(format!(
                "warning: {}: cannot find its main repo ({why}); skipped",
                seed.display()
            )),
        }
    }
    RepoCollection {
        repos: set.into_iter().collect(),
        warnings,
    }
}

/// Fallback seeds when the scan gave no groups: parents of `.git` directories under `roots`. It does not
/// descend into `.git`, `node_modules`, `target` or any directory whose own `.git` is a file (a linked
/// worktree), so it never walks the inside of a worktree.
pub fn git_dir_parents(roots: &[PathBuf], max_depth: usize) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, max: usize, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let git = dir.join(".git");
        match std::fs::symlink_metadata(&git) {
            Ok(m) if m.is_dir() => out.push(dir.to_path_buf()),
            Ok(m) if m.is_file() => return,
            _ => {}
        }
        if depth >= max {
            return;
        }
        for e in rd.flatten() {
            let name = e.file_name();
            if matches!(name.to_str(), Some(".git" | "node_modules" | "target")) {
                continue;
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&e.path(), depth + 1, max, out);
            }
        }
    }
    let mut out = Vec::new();
    for r in roots {
        walk(r, 0, max_depth, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

/// Seeds from scan groups (`Group.repo`), falling back to `.git` directories when the scan has none.
pub fn repo_seeds(report: &disk_cleanup_scan::scan::ScanReport, roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut seeds: Vec<PathBuf> = report
        .groups
        .iter()
        .filter(|g| g.kind != disk_cleanup_scan::scan::GroupKind::Loose)
        .map(|g| g.repo.clone())
        .collect();
    seeds.sort();
    seeds.dedup();
    if seeds.is_empty() {
        return git_dir_parents(roots, 6);
    }
    seeds
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_should_read_branch_detached_locked_and_prunable_records() {
        let raw = b"worktree /r\0HEAD abc\0branch refs/heads/main\0\0worktree /r/.claude/worktrees/agent-1\0HEAD def\0detached\0locked why now\0prunable gone\0\0";
        let e = parse_porcelain_z(raw);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].branch.as_deref(), Some("refs/heads/main"));
        assert!(e[1].detached && e[1].prunable);
        assert_eq!(e[1].locked.as_deref(), Some("why now"));
        assert_eq!(e[1].head.as_deref(), Some("def"));
    }

    #[test]
    fn parse_should_keep_a_path_with_a_newline() {
        let e = parse_porcelain_z(b"worktree /r/a\nb\0HEAD x\0\0");
        assert_eq!(e[0].path, PathBuf::from("/r/a\nb"));
    }

    #[test]
    fn agent_path_should_be_exactly_one_level_below_the_agent_dir() {
        let r = Path::new("/r");
        assert!(is_agent_worktree_path(
            r,
            Path::new("/r/.claude/worktrees/agent-a41")
        ));
        assert!(!is_agent_worktree_path(
            r,
            Path::new("/r/.claude/worktrees/feat-x")
        ));
        assert!(!is_agent_worktree_path(
            r,
            Path::new("/r/.claude/worktrees/agent-")
        ));
        assert!(!is_agent_worktree_path(
            r,
            Path::new("/r/.claude/worktrees/agent-a/sub")
        ));
        assert!(!is_agent_worktree_path(r, Path::new("/r/agent-a")));
    }
}

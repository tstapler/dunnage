//! disk-cleanup: reclaim disk space by clearing package-manager/build caches
//! and removing stale Claude Code agent git worktrees.
//!
//! Staleness/eligibility decisions live in `disk_cleanup_core` so they can be
//! unit tested without touching a real filesystem or git repo; this crate
//! only does I/O (walking directories, shelling out to git/du/docker/etc.)
//! and reports what it did or would do.

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Parser;
use disk_cleanup_core::{
    worktree_verdict, CacheTarget, Verdict, WorktreeCandidate, LINUX_CACHE_TARGETS,
    MACOS_CACHE_TARGETS,
};

/// Reclaim disk space by clearing caches and removing stale Claude Code agent worktrees.
#[derive(Parser)]
#[command(name = "disk-cleanup", version)]
struct Cli {
    /// Show what would be removed without removing anything.
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Print why each candidate was kept, not just what was removed.
    #[arg(short, long)]
    verbose: bool,

    /// A worktree counts as stale once its last commit is at least this many days old.
    #[arg(long, default_value_t = 14)]
    stale_days: u32,
}

/// Whether this run actually mutates the filesystem/git state, or only
/// reports what it would do. Passed around instead of a bare `bool` so call
/// sites read as `Mode::DryRun`/`Mode::Real` rather than an unlabeled `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Real,
    DryRun,
}

impl Mode {
    fn from_dry_run_flag(dry_run: bool) -> Self {
        if dry_run {
            Mode::DryRun
        } else {
            Mode::Real
        }
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mode = Mode::from_dry_run_flag(cli.dry_run);

    println!("=== disk-cleanup ===");
    if mode == Mode::DryRun {
        println!("(dry run — nothing will be deleted)");
    }
    print_df("Before");

    let home =
        PathBuf::from(std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME is not set"))?);

    let targets: &[CacheTarget] = if cfg!(target_os = "macos") {
        MACOS_CACHE_TARGETS
    } else {
        LINUX_CACHE_TARGETS
    };
    remove_cache_targets(&home, targets, mode);
    run_external_cache_steps(mode);
    cleanup_worktrees(&home, cli.stale_days, mode, cli.verbose);

    print_df("After");
    Ok(())
}

fn print_df(label: &str) {
    println!();
    if let Ok(out) = Command::new("df").args(["-h", "/"]).output() {
        if let Some(line) = String::from_utf8_lossy(&out.stdout).lines().nth(1) {
            println!("{label}: {line}");
        }
    }
}

fn human_size(path: &Path) -> String {
    Command::new("du")
        .args(["-sh", &path.to_string_lossy()])
        .output()
        .ok()
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()
                .map(str::to_string)
        })
        .unwrap_or_else(|| "?".to_string())
}

fn remove_cache_targets(home: &Path, targets: &[CacheTarget], mode: Mode) {
    for target in targets {
        let path = home.join(target.home_relative);
        if !path.exists() {
            continue;
        }
        let size = human_size(&path);
        match mode {
            Mode::DryRun => println!(
                "[dry-run] would remove {} ({size}) — {}",
                target.label,
                path.display()
            ),
            Mode::Real => match std::fs::remove_dir_all(&path) {
                Ok(()) => println!("removed {} ({size})", target.label),
                Err(err) => eprintln!("failed to remove {}: {err}", path.display()),
            },
        }
    }
}

/// Steps that clear caches via the owning tool's own CLI rather than a bare
/// `rm -rf` (so the tool stays consistent, e.g. `go clean -cache` instead of
/// guessing GOCACHE), plus Docker/apt/Homebrew. Skipped when the tool isn't
/// installed.
fn run_external_cache_steps(mode: Mode) {
    run_if_present("go", &["clean", "-cache"], mode, "Go build cache");
    run_if_present("pip3", &["cache", "purge"], mode, "pip cache");

    if cfg!(target_os = "macos") {
        run_if_present("brew", &["cleanup", "--prune=all"], mode, "Homebrew cache");
        run_if_present(
            "xcrun",
            &["simctl", "delete", "unavailable"],
            mode,
            "unavailable iOS simulators",
        );
    } else {
        if which("apt-get") {
            run_command("sudo", &["apt-get", "clean"], mode, "apt cache");
        }
        if which("docker")
            && Command::new("docker")
                .arg("info")
                .output()
                .is_ok_and(|o| o.status.success())
        {
            run_command(
                "docker",
                &["system", "prune", "-f"],
                mode,
                "Docker unused data",
            );
        }
    }
}

fn which(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn run_if_present(bin: &str, args: &[&str], mode: Mode, label: &str) {
    if which(bin) {
        run_command(bin, args, mode, label);
    }
}

fn run_command(bin: &str, args: &[&str], mode: Mode, label: &str) {
    if mode == Mode::DryRun {
        println!("[dry-run] would run: {bin} {}", args.join(" "));
        return;
    }
    match Command::new(bin).args(args).output() {
        Ok(out) if out.status.success() => println!("cleared {label}"),
        Ok(out) => eprintln!(
            "{bin} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(err) => eprintln!("failed to run {bin}: {err}"),
    }
}

struct WorktreeEntry {
    path: PathBuf,
    locked: bool,
}

fn parse_worktree_list(porcelain: &str) -> Vec<WorktreeEntry> {
    porcelain
        .split("\n\n")
        .filter_map(|block| {
            let mut lines = block.lines();
            let path = lines.next()?.strip_prefix("worktree ")?;
            let locked = lines.any(|l| l == "locked" || l.starts_with("locked "));
            Some(WorktreeEntry {
                path: PathBuf::from(path),
                locked,
            })
        })
        .collect()
}

fn git_stdout(cwd: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn current_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed())
}

fn is_agent_worktree(path: &Path) -> bool {
    path.to_string_lossy().contains("/.claude/worktrees/agent-")
}

/// Finds Claude Code agent worktrees under `$HOME/dotfiles` and `$HOME/code`,
/// removing any that are stale per [`worktree_verdict`]. A worktree is never
/// removed while locked or dirty, regardless of age — see the core crate's
/// tests for the exact precedence.
fn cleanup_worktrees(home: &Path, stale_days: u32, mode: Mode, verbose: bool) {
    println!("\n▶ Stale Claude agent worktrees (.claude/worktrees/agent-*, >{stale_days}d since last commit)");
    let mut any = false;
    for root in [home.join("dotfiles"), home.join("code")] {
        for repo in find_git_repos(&root) {
            any |= cleanup_worktrees_in_repo(&repo, stale_days, mode, verbose);
        }
    }
    if !any {
        println!("  none found");
    }
}

/// Returns whether any agent worktree was found in this repo (removed or not).
fn cleanup_worktrees_in_repo(repo: &Path, stale_days: u32, mode: Mode, verbose: bool) -> bool {
    let Some(list) = git_stdout(repo, &["worktree", "list", "--porcelain"]) else {
        return false;
    };

    let mut found_any = false;
    let mut removed_any = false;
    for entry in parse_worktree_list(&list) {
        if !is_agent_worktree(&entry.path) {
            continue;
        }
        found_any = true;
        if handle_worktree_candidate(repo, &entry, stale_days, mode, verbose) {
            removed_any = true;
        }
    }
    if removed_any && mode == Mode::Real {
        let _ = git_stdout(repo, &["worktree", "prune"]);
    }
    found_any
}

/// Returns whether the worktree was (or would be) removed.
fn handle_worktree_candidate(
    repo: &Path,
    entry: &WorktreeEntry,
    stale_days: u32,
    mode: Mode,
    verbose: bool,
) -> bool {
    let dirty =
        git_stdout(&entry.path, &["status", "--porcelain"]).is_some_and(|s| !s.trim().is_empty());
    let last_commit_epoch = git_stdout(&entry.path, &["log", "-1", "--format=%ct"])
        .and_then(|s| s.trim().parse::<i64>().ok());
    let candidate = WorktreeCandidate {
        path: entry.path.clone(),
        last_commit_epoch,
        locked: entry.locked,
        dirty,
    };

    match worktree_verdict(&candidate, current_epoch(), stale_days) {
        Verdict::Stale => remove_stale_worktree(repo, &entry.path, mode),
        Verdict::SkipLocked => {
            println!("  skip {} (locked)", entry.path.display());
            false
        }
        Verdict::SkipDirty => {
            println!("  skip {} (uncommitted changes)", entry.path.display());
            false
        }
        Verdict::SkipTooRecent => {
            if verbose {
                println!(
                    "  skip {} (under {stale_days}d threshold)",
                    entry.path.display()
                );
            }
            false
        }
    }
}

fn remove_stale_worktree(repo: &Path, path: &Path, mode: Mode) -> bool {
    let size = human_size(path);
    match mode {
        Mode::DryRun => {
            println!(
                "  [dry-run] would remove worktree {} ({size})",
                path.display()
            );
            false
        }
        Mode::Real => {
            if git_stdout(
                repo,
                &["worktree", "remove", "--force", &path.to_string_lossy()],
            )
            .is_some()
            {
                println!("  removed worktree {} ({size})", path.display());
                true
            } else {
                eprintln!("  failed to remove worktree {}", path.display());
                false
            }
        }
    }
}

/// Repos containing a `.claude/worktrees/agent-*` path — bounded depth since
/// this only needs to reach `~/code/<host>/<owner>/<repo>` (per this
/// machine's repo-placement convention), not walk arbitrary project trees.
fn find_git_repos(root: &Path) -> Vec<PathBuf> {
    const MAX_DEPTH: usize = 4;
    let mut repos = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            // `file_type` does not follow symlinks, so links are never descended.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            // A linked worktree's `.git` is a *file* pointing back at the main
            // repo's `.git/worktrees/<name>`, not a directory. Matching it too
            // would make `git worktree list` run once per worktree instead of
            // once per repo, multiplying every result by its own worktree count.
            if entry.file_name() == ".git" {
                repos.push(dir.clone());
            } else if depth + 1 < MAX_DEPTH {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
    repos
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_locked_and_unlocked_worktrees() {
        let porcelain = "worktree /repo\nHEAD abc\nbranch refs/heads/master\n\n\
                          worktree /repo/.claude/worktrees/agent-1\nHEAD def\nbranch refs/heads/foo\nlocked\n\n\
                          worktree /repo/.claude/worktrees/agent-2\nHEAD ghi\nbranch refs/heads/bar\n";
        let entries = parse_worktree_list(porcelain);
        assert_eq!(entries.len(), 3);
        assert!(!entries[0].locked);
        assert!(entries[1].locked);
        assert!(!entries[2].locked);
    }

    #[test]
    fn recognizes_agent_worktree_paths() {
        assert!(is_agent_worktree(Path::new(
            "/repo/.claude/worktrees/agent-abc123"
        )));
        assert!(!is_agent_worktree(Path::new("/repo")));
    }

    /// Regression test: a linked worktree's `.git` is a file, not a
    /// directory, and must not be mistaken for a second repo — otherwise
    /// `git worktree list` runs once per worktree and every result is
    /// duplicated by the worktree count.
    #[test]
    fn find_git_repos_ignores_worktree_git_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let worktree = repo.join(".claude/worktrees/agent-1");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            "gitdir: ../../../.git/worktrees/agent-1\n",
        )
        .unwrap();

        let repos = find_git_repos(tmp.path());
        assert_eq!(repos, vec![repo]);
    }
}

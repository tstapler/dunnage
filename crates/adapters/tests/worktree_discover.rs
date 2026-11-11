#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names,
    non_snake_case
)]
//! Worktree discovery over real git repos in tempdirs (Story 3.1.1; validation T-146).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use disk_cleanup_adapters::targets::worktree_discover::{
    collect_repos, discover_worktrees, git_dir_parents,
};
use disk_cleanup_adapters::RealCommandRunner;
use disk_cleanup_core::ports::{Cmd, CmdOutput, CommandRunner};
use disk_cleanup_core::testing::FakeCommandRunner;
use disk_cleanup_core::{ApplyToken, DeleteError, WorktreeRemoval};

/// Wraps the real runner and records every argv.
struct Counting {
    inner: RealCommandRunner,
    log: Mutex<Vec<Vec<String>>>,
}

impl Counting {
    fn new() -> Counting {
        Counting {
            inner: RealCommandRunner::new(),
            log: Mutex::new(Vec::new()),
        }
    }
    fn count(&self, needle: &[&str]) -> usize {
        let log = self.log.lock().unwrap();
        log.iter()
            .filter(|a| a.windows(needle.len()).any(|w| w == needle))
            .count()
    }
}

impl CommandRunner for Counting {
    fn exists(&self, p: &str) -> bool {
        self.inner.exists(p)
    }
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, DeleteError> {
        let mut argv = vec![cmd.program.to_string_lossy().into_owned()];
        argv.extend(cmd.args.iter().map(|a| a.to_string_lossy().into_owned()));
        self.log.lock().unwrap().push(argv);
        self.inner.run(cmd)
    }
    fn run_mutating(&self, c: &Cmd, p: &ApplyToken) -> Result<CmdOutput, DeleteError> {
        self.inner.run_mutating(c, p)
    }
    fn remove_worktree(
        &self,
        w: &WorktreeRemoval,
        p: &ApplyToken,
    ) -> Result<CmdOutput, DeleteError> {
        self.inner.remove_worktree(w, p)
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@e",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn repo(tmp: &Path, name: &str) -> PathBuf {
    let r = tmp.join(name);
    fs::create_dir_all(&r).unwrap();
    git(&r, &["init", "-q", "-b", "main"]);
    git(&r, &["commit", "-q", "--allow-empty", "-m", "init"]);
    r
}

fn add(r: &Path, rel: &str) -> PathBuf {
    git(r, &["worktree", "add", "-q", "--detach", rel]);
    r.join(rel)
}

fn key(a: &[String]) -> Vec<&str> {
    std::iter::once("git")
        .chain(a.iter().map(String::as_str))
        .collect()
}

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = fs::canonicalize(t.path()).unwrap();
    (t, p)
}

#[test]
fn discovery_should_list_once_and_return_two_candidates_When_two_agent_worktrees() {
    let (_t, p) = tmp();
    let r = repo(&p, "r");
    let a = add(&r, ".claude/worktrees/agent-a41");
    let b = add(&r, ".claude/worktrees/agent-b07");
    let runner = Counting::new();

    let d = discover_worktrees(&runner, std::slice::from_ref(&r));

    assert_eq!(runner.count(&["worktree", "list", "--porcelain", "-z"]), 1);
    assert_eq!(runner.log.lock().unwrap().len(), 1, "only the one list ran");
    let mut got: Vec<_> = d.sites.iter().map(|s| s.entry.path.clone()).collect();
    got.sort();
    assert_eq!(got, vec![a, b]);
    assert!(d
        .sites
        .iter()
        .all(|s| s.main_repo == r && s.ident.is_some()));
    assert!(d.warnings.is_empty());
}

#[test]
fn discovery_should_skip_a_worktree_not_named_agent_When_it_sits_in_the_same_directory() {
    let (_t, p) = tmp();
    let r = repo(&p, "r");
    add(&r, ".claude/worktrees/agent-a41");
    add(&r, ".claude/worktrees/feat-buildclean-bun");
    add(&r, "elsewhere/agent-z");

    let d = discover_worktrees(&Counting::new(), &[r]);

    assert_eq!(d.sites.len(), 1);
    assert!(d.sites[0].entry.path.ends_with("agent-a41"));
}

#[test]
fn discovery_should_return_exactly_eight_When_each_worktree_holds_200_subdirs_and_a_nested_git_dir()
{
    let (_t, p) = tmp();
    let r = repo(&p, "r");
    for i in 0..8 {
        let w = add(&r, &format!(".claude/worktrees/agent-{i:02}"));
        for j in 0..200 {
            fs::create_dir_all(w.join(format!("d{j}"))).unwrap();
        }
        fs::create_dir_all(w.join("d0/.git")).unwrap();
    }
    let runner = Counting::new();

    let d = discover_worktrees(&runner, std::slice::from_ref(&r));

    assert_eq!(d.sites.len(), 8);
    assert_eq!(runner.count(&["worktree", "list", "--porcelain", "-z"]), 1);
    assert_eq!(
        runner.count(&["status"]),
        0,
        "discovery issues no per-worktree probe"
    );
    assert_eq!(runner.count(&["log"]), 0);
    // the fallback walk never enters a worktree either
    assert_eq!(git_dir_parents(std::slice::from_ref(&p), 6), vec![r]);
}

#[test]
fn collect_repos_should_name_the_main_repo_once_When_a_repo_and_its_worktree_are_both_seeds() {
    let (_t, p) = tmp();
    let r = repo(&p, "r");
    let w = add(&r, ".claude/worktrees/agent-a41");
    let other = repo(&p, "o");

    let c = collect_repos(&Counting::new(), &[r.clone(), w, other.clone(), r.clone()]);

    assert_eq!(c.repos, vec![other, r]);
    assert!(c.warnings.is_empty(), "{:?}", c.warnings);
}

#[test]
fn collect_repos_should_warn_and_continue_When_a_seed_is_not_a_repo() {
    let (_t, p) = tmp();
    let r = repo(&p, "r");
    let not_repo = p.join("plain");
    fs::create_dir_all(&not_repo).unwrap();

    let c = collect_repos(&Counting::new(), &[not_repo.clone(), r.clone()]);

    assert_eq!(c.repos, vec![r]);
    assert_eq!(c.warnings.len(), 1);
    assert!(c.warnings[0].starts_with("warning:") && c.warnings[0].contains("plain"));
}

#[test]
fn discovery_should_warn_and_still_find_the_others_When_one_repo_exits_128() {
    let (_t, p) = tmp();
    let (r1, bad, r3) = (repo(&p, "r1"), p.join("bad"), repo(&p, "r3"));
    let a1 = add(&r1, ".claude/worktrees/agent-1");
    let a3 = add(&r3, ".claude/worktrees/agent-3");
    let list = |r: &Path| -> Vec<String> {
        [
            "-C",
            r.to_str().unwrap(),
            "worktree",
            "list",
            "--porcelain",
            "-z",
        ]
        .map(String::from)
        .to_vec()
    };
    let fake = FakeCommandRunner::new();
    let real = RealCommandRunner::new();
    for (r, w) in [(&r1, &a1), (&r3, &a3)] {
        let out = real.run(&Cmd::new("git", &list(r)[..], false)).unwrap();
        fake.on(&key(&list(r)), Ok(out));
        assert!(w.exists());
    }
    fake.on(
        &key(&list(&bad)),
        Err(DeleteError::CommandFailed {
            status: 128,
            message: "fatal: not a git repository".into(),
        }),
    );

    let d = discover_worktrees(&fake, &[r1, bad.clone(), r3]);

    assert_eq!(d.sites.len(), 2);
    assert_eq!(d.warnings.len(), 1);
    assert!(d.warnings[0].starts_with("warning:") && d.warnings[0].contains(bad.to_str().unwrap()));
    assert_eq!(fake.calls().len(), 3, "no find or other guess was issued");
}

#[test]
fn discovery_should_mark_a_locked_entry_and_a_prunable_one() {
    let (_t, p) = tmp();
    let r = repo(&p, "r");
    let a = add(&r, ".claude/worktrees/agent-lock");
    let b = add(&r, ".claude/worktrees/agent-gone");
    git(
        &r,
        &["worktree", "lock", "--reason", "mine", a.to_str().unwrap()],
    );
    fs::remove_dir_all(&b).unwrap();

    let d = discover_worktrees(&Counting::new(), &[r]);

    let by = |n: &str| d.sites.iter().find(|s| s.entry.path.ends_with(n)).unwrap();
    assert_eq!(by("agent-lock").entry.locked.as_deref(), Some("mine"));
    assert!(by("agent-gone").entry.prunable);
    assert!(by("agent-gone").ident.is_none());
}

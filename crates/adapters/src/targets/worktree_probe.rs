//! Worktree probes (Story 3.1.2): everything `decide_worktree` needs, gathered read-only. The staged order
//! mirrors the decision: a stage runs only when every cheaper stage before it still allows the worktree, so a
//! locked, dirty or young worktree never pays for a tree walk, and the facts of a stage that did not run are
//! left "unknown" (the decision has already answered by then).
//!
//! Git is asked through the `CommandRunner` allowlist (`status`, `log`, `branch -r --contains`), never
//! through a shell. `git status` may refresh the index and move the admin entry's mtime, so the admin entry
//! is read before the first git command.

use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dunnage_core::guard::{decide_path_common, Facts, GuardDecision};
use dunnage_core::ports::{Cmd, CommandRunner, SystemProbe};
use dunnage_core::roots::RootSet;
use dunnage_core::targets::worktree_guard::{
    decide_worktree, IgnoredFiles, PushState, WorktreeCandidate, WorktreeGuard, WorktreeOpts,
    WORKTREE_MIN_AGE,
};
use dunnage_core::{Identity, Kind, RootDev, Tri};
use dunnage_scan::classify::classify_path;

use crate::dirops::{open_verified_root, DirOps};
use crate::discover_build::symlink_between;
use crate::mtime_probe::{
    probe_newest_mtime, to_system_time, unix_secs, NewestStore, ProbeTarget, Recency,
};
use crate::targets::worktree_discover::WorktreeSite;
use crate::treestat::{measure, MeasureOpts};

/// What the probes need from outside.
pub struct ProbeEnv<'a, D> {
    pub runner: &'a dyn CommandRunner,
    pub probe: &'a dyn SystemProbe,
    pub ops: &'a D,
    pub roots: &'a RootSet,
}

/// The facts of one worktree and the path facts the decision takes besides them.
#[derive(Debug)]
pub struct WorktreeProbed {
    pub candidate: WorktreeCandidate,
    /// `None` when the path no longer exists.
    pub facts: Option<Facts>,
    /// The identity the decision compares with: the one taken at discovery.
    pub planned: Identity,
}

fn git(wt: &Path, args: &[&str]) -> Cmd {
    let mut argv: Vec<OsString> = vec!["-C".into(), wt.as_os_str().to_owned()];
    argv.extend(args.iter().map(OsString::from));
    Cmd::new("git", argv, false)
}

fn path_facts<D>(env: &ProbeEnv<'_, D>, path: &Path) -> Option<(Facts, std::fs::Metadata)> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let ident = Identity::of(&meta);
    let root = env.roots.scan_roots().iter().find(|r| path.starts_with(r));
    let root_dev = root
        .and_then(|r| std::fs::metadata(r).ok())
        .map_or(ident.dev, |m| m.dev());
    let mounts = env.probe.mounts().unwrap_or_default();
    let facts = Facts {
        kind: Kind::Worktree,
        path: path.to_path_buf(),
        ident,
        is_symlink: meta.file_type().is_symlink() || root.is_some_and(|r| symlink_between(r, path)),
        root_dev: RootDev(root_dev),
        is_mount_point: mounts.iter().any(|m| m.mount_point == path),
    };
    Some((facts, meta))
}

/// mtime (Unix seconds) of `<git-common-dir>/worktrees/<name>`, found through the `gitdir:` line of the
/// worktree's `.git` file.
fn admin_mtime(wt: &Path) -> Option<i64> {
    let text = std::fs::read_to_string(wt.join(".git")).ok()?;
    let dir = text.lines().find_map(|l| l.strip_prefix("gitdir: "))?;
    let dir = Path::new(dir.trim());
    let dir = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        wt.join(dir)
    };
    std::fs::metadata(dir).ok().map(|m| m.mtime())
}

fn run_ok(env_runner: &dyn CommandRunner, cmd: &Cmd) -> Option<String> {
    env_runner.run(cmd).ok().map(|o| o.stdout_lossy())
}

/// Whether `git status --untracked-files=normal` lists anything. A failing git counts as dirty.
fn is_dirty(runner: &dyn CommandRunner, wt: &Path) -> bool {
    run_ok(
        runner,
        &git(
            wt,
            &["status", "--porcelain=v1", "--untracked-files=normal"],
        ),
    )
    .is_none_or(|out| !out.trim().is_empty())
}

pub fn last_commit(runner: &dyn CommandRunner, wt: &Path) -> Option<i64> {
    run_ok(runner, &git(wt, &["log", "-1", "--format=%ct"]))?
        .trim()
        .parse()
        .ok()
}

/// `Pushed` when a remote-tracking branch contains HEAD. The unpushed count is not known: the allowlist has
/// no `rev-list` row.
fn push_state(runner: &dyn CommandRunner, wt: &Path, head: Option<&str>) -> PushState {
    let Some(head) = head else {
        return PushState::Unknown;
    };
    match run_ok(runner, &git(wt, &["branch", "-r", "--contains", head])) {
        Some(out) if !out.trim().is_empty() => PushState::Pushed,
        Some(_) => PushState::Unpushed { commits: None },
        None => PushState::Unknown,
    }
}

/// Unquotes one path of `git status --porcelain=v1`: a path with special characters is C-quoted.
pub fn unquote_porcelain_path(raw: &str) -> Vec<u8> {
    let Some(inner) = raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) else {
        return raw.as_bytes().to_vec();
    };
    let b = inner.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 >= b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        match b[i] {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'r' => out.push(b'\r'),
            b'v' => out.push(11),
            c @ b'0'..=b'3'
                if i + 2 < b.len() && b[i + 1].is_ascii_digit() && b[i + 2].is_ascii_digit() =>
            {
                let v = u32::from(c - b'0') * 64
                    + u32::from(b[i + 1] - b'0') * 8
                    + u32::from(b[i + 2] - b'0');
                out.push(u8::try_from(v).unwrap_or(b'?'));
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    out
}

/// The `!! path` entries of `git status --porcelain=v1 --ignored=matching`, relative to the worktree.
pub fn parse_ignored(stdout: &str) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    stdout
        .lines()
        .filter_map(|l| l.strip_prefix("!! "))
        .map(|raw| {
            let bytes = unquote_porcelain_path(raw);
            PathBuf::from(OsString::from_vec(bytes))
        })
        .collect()
}

/// Counts and sizes the ignored entries that `git worktree remove` would delete. `None` when git failed.
fn ignored_files<D: DirOps>(
    env: &ProbeEnv<'_, D>,
    wt: &Path,
    root_dev: RootDev,
) -> Option<IgnoredFiles> {
    let out = run_ok(
        env.runner,
        &git(
            wt,
            &[
                "status",
                "--porcelain=v1",
                "--ignored=matching",
                "--untracked-files=normal",
            ],
        ),
    )?;
    let mut total = IgnoredFiles::default();
    for rel in parse_ignored(&out) {
        let rel = PathBuf::from(rel.to_string_lossy().trim_end_matches('/'));
        let full = wt.join(&rel);
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        let (count, bytes) = if meta.is_dir() {
            let Ok(root) = open_verified_root(env.ops, &full, Identity::of(&meta)) else {
                // unreadable: it still counts as something git would delete
                total.entries += 1;
                continue;
            };
            let s = measure(env.ops, root.dir(), root_dev, MeasureOpts::default());
            (s.entries.saturating_sub(1).max(1), s.allocated.0)
        } else {
            (1, meta.blocks() * 512)
        };
        if meta.is_dir() && classify_path(&full).is_some() {
            total.build_output_bytes += bytes;
        } else {
            total.entries += count;
            total.bytes += bytes;
        }
    }
    Some(total)
}

/// Newest mtime under the tree (Unix seconds), or `None` when it could not be established.
fn newest_mtime<D: DirOps>(
    env: &ProbeEnv<'_, D>,
    f: &Facts,
    meta: &std::fs::Metadata,
    now: i64,
    store: &mut NewestStore,
) -> Option<i64> {
    let floor_secs = i64::try_from(WORKTREE_MIN_AGE.as_secs()).unwrap_or(i64::MAX);
    let own = (meta.mtime(), u32::try_from(meta.mtime_nsec()).unwrap_or(0));
    let target = ProbeTarget {
        path: &f.path,
        ident: f.ident,
        own,
        root_dev: f.root_dev,
    };
    let floor = to_system_time(now - floor_secs, 0);
    match probe_newest_mtime(env.ops, target, floor, store)
        .ok()?
        .recency
    {
        Recency::Young { newest, .. } => Some(unix_secs(newest)),
        Recency::Old { newest } => Some(newest.map_or(own.0, unix_secs)),
        Recency::Unknown { .. } => None,
    }
}

/// Gathers the facts of `site` in stages (see the module doc). `min_age` is the flag as given.
pub fn probe_worktree<D: DirOps>(
    env: &ProbeEnv<'_, D>,
    site: &WorktreeSite,
    now: i64,
    min_age: Duration,
    store: &mut NewestStore,
) -> WorktreeProbed {
    let path = &site.entry.path;
    let mut c = WorktreeCandidate::new(path, None, site.entry.locked.is_some(), false);
    c.detached = site.entry.detached;
    let current = path_facts(env, path);
    let planned = site
        .ident
        .or_else(|| current.as_ref().map(|(f, _)| f.ident))
        .unwrap_or(Identity { dev: 0, ino: 0 });
    let Some((facts, meta)) = current else {
        return WorktreeProbed {
            candidate: c,
            facts: None,
            planned,
        };
    };
    let done = |c: WorktreeCandidate, facts: Facts| WorktreeProbed {
        candidate: c,
        facts: Some(facts),
        planned,
    };
    if matches!(
        decide_path_common(&facts, env.roots),
        GuardDecision::Skip(_)
    ) || facts.ident != planned
        || c.locked
    {
        return done(c, facts);
    }
    c.admin_mtime = admin_mtime(path);
    c.dirty = is_dirty(env.runner, path);
    if c.dirty {
        return done(c, facts);
    }
    let (in_use, holder) = env.probe.cwd_in_use(path);
    c.in_use = in_use;
    c.holder = holder;
    if in_use != Tri::NotRunning {
        return done(c, facts);
    }
    c.last_commit_epoch = last_commit(env.runner, path);
    let need = i64::try_from(min_age.as_secs()).unwrap_or(i64::MAX);
    if c.last_commit_epoch.is_none_or(|t| now - t < need) {
        return done(c, facts);
    }
    c.newest_mtime = newest_mtime(env, &facts, &meta, now, store);
    let youngest = [c.last_commit_epoch, c.newest_mtime, c.admin_mtime]
        .into_iter()
        .map(|t| t.unwrap_or(now))
        .max()
        .unwrap_or(now);
    if now - youngest < i64::try_from(WORKTREE_MIN_AGE.as_secs()).unwrap_or(i64::MAX) {
        return done(c, facts);
    }
    c.push = push_state(env.runner, path, site.entry.head.as_deref());
    if c.push == PushState::Pushed {
        c.ignored = ignored_files(env, path, facts.root_dev);
    }
    done(c, facts)
}

/// Probes `site` and decides. The same call serves plan time and, with a fresh `site.ident`-carrying site,
/// the re-check just before removal (Story 3.1.3).
pub fn guard_worktree<D: DirOps>(
    env: &ProbeEnv<'_, D>,
    site: &WorktreeSite,
    now: i64,
    opts: WorktreeOpts,
    store: &mut NewestStore,
) -> (WorktreeProbed, WorktreeGuard) {
    let probed = probe_worktree(env, site, now, opts.min_age, store);
    let guard = decide_worktree(
        &probed.candidate,
        probed.facts.as_ref(),
        probed.planned,
        env.roots,
        now,
        opts,
    );
    (probed, guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ignored_should_keep_only_bang_lines_and_unquote_paths() {
        let out = "?? new.txt\n!! .env\n!! target/\n!! \"sp\\303\\251cial \\\"q\\\".txt\"\n M a\n";
        let got = parse_ignored(out);
        assert_eq!(
            got,
            vec![
                PathBuf::from(".env"),
                PathBuf::from("target/"),
                PathBuf::from("spécial \"q\".txt"),
            ]
        );
    }

    #[test]
    fn unquote_should_leave_a_plain_path_and_handle_escapes() {
        assert_eq!(unquote_porcelain_path("a b"), b"a b");
        assert_eq!(unquote_porcelain_path("\"a\\tb\\\\c\""), b"a\tb\\c");
    }
}

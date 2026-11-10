//! Parallel directory walk that attributes inodes to repos/worktrees and to source versus
//! rebuildable build dirs (`node_modules`, cargo `target`) and `.git`.
//!
//! The walk only records per-directory counters plus a few marker flags. Classification
//! (build dir, git root, worktree-of, duplicate cluster) is a pure pass over that map, so it is
//! deterministic regardless of thread scheduling. The one exception is hard links: with several
//! threads the directory that "owns" a multiply-linked inode is whichever thread saw it first.

use crate::btrfs_tree::BtrfsTreeBackend;
use crate::classify::{classify_dir, ParentFacts};
use crate::estimate::Stats;
use crate::index::{Backend, CollectRequest, DirInfo};
use crate::walker::{CacheConfig, WalkerBackend};
use crate::{Error, Result};
use serde::Serialize;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_LOCKFILE_BYTES: u64 = 32 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendChoice {
    /// Tree search when running as root on btrfs, otherwise the walker.
    Auto,
    Walker,
    BtrfsTree,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ScanOptions {
    pub roots: Vec<PathBuf>,
    pub backend: BackendChoice,
    /// Descend into other mounts and btrfs subvolumes (distinct `st_dev`). Walker only.
    pub cross_fs: bool,
    pub max_duration: Option<Duration>,
    /// Walker threads; 0 picks 3x CPUs (capped at 64) because the walk is I/O-bound.
    pub threads: usize,
    /// Walker incremental cache; `None` disables it.
    pub cache: Option<CacheConfig>,
}

impl ScanOptions {
    /// Defaults: auto backend, single filesystem, no time limit, auto threads, no cache.
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            backend: BackendChoice::Auto,
            cross_fs: false,
            max_duration: None,
            threads: 0,
            cache: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Source,
    NodeModules,
    CargoTarget,
    Git,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildKind {
    NodeModules,
    CargoTarget,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CategoryStats {
    pub source: Stats,
    pub node_modules: Stats,
    pub cargo_target: Stats,
    pub git: Stats,
}

impl CategoryStats {
    pub fn get_mut(&mut self, c: Category) -> &mut Stats {
        match c {
            Category::Source => &mut self.source,
            Category::NodeModules => &mut self.node_modules,
            Category::CargoTarget => &mut self.cargo_target,
            Category::Git => &mut self.git,
        }
    }

    pub fn merge(&mut self, o: &CategoryStats) {
        self.source.merge(&o.source);
        self.node_modules.merge(&o.node_modules);
        self.cargo_target.merge(&o.cargo_target);
        self.git.merge(&o.git);
    }

    pub fn combined(&self) -> Stats {
        let mut all = Stats::default();
        for s in [
            &self.source,
            &self.node_modules,
            &self.cargo_target,
            &self.git,
        ] {
            all.merge(s);
        }
        all
    }

    pub fn metadata_est_bytes(&self) -> u64 {
        [
            &self.source,
            &self.node_modules,
            &self.cargo_target,
            &self.git,
        ]
        .iter()
        .map(|s| s.metadata_cost().total())
        .sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupKind {
    /// A directory with a `.git` directory.
    Checkout,
    /// A directory whose `.git` is a file pointing into `<repo>/.git/worktrees/`.
    Worktree,
    /// Not inside any git checkout; grouped by first path component under the scan root.
    Loose,
}

#[derive(Debug, Clone, Serialize)]
pub struct Group {
    pub path: PathBuf,
    pub kind: GroupKind,
    /// Main checkout for worktrees, otherwise `path`. Rows with equal `repo` are one family.
    pub repo: PathBuf,
    pub stats: CategoryStats,
    pub inodes: u64,
    pub metadata_est_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Family {
    pub repo: PathBuf,
    pub checkouts: usize,
    pub stats: CategoryStats,
    pub inodes: u64,
    pub metadata_est_bytes: u64,
}

/// One discovered `node_modules` (sibling `package.json`) or `target` (sibling `Cargo.toml`).
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct BuildRoot {
    pub path: PathBuf,
    pub kind: BuildKind,
    pub group: PathBuf,
    pub stats: Stats,
    pub metadata_est_bytes: u64,
    /// Seconds since the directory's own mtime. Directory mtime only changes when direct
    /// entries are added or removed, so this under-reports recent activity deep inside.
    pub age_secs: u64,
    /// Hash of the sibling lockfile, if one was found and readable.
    pub lockfile_hash: Option<u64>,
}

/// Build dirs of one kind sharing the same lockfile content: heuristic "same tree" evidence.
#[derive(Debug, Clone, Serialize)]
pub struct DuplicateCluster {
    pub kind: BuildKind,
    pub lockfile_hash: u64,
    pub members: Vec<PathBuf>,
    pub min_inodes: u64,
    pub max_inodes: u64,
    pub total_inodes: u64,
    /// total minus the largest member: what keeping one copy would free.
    pub redundant_inodes: u64,
    pub redundant_metadata_est_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct ScanReport {
    pub roots: Vec<PathBuf>,
    pub groups: Vec<Group>,
    pub families: Vec<Family>,
    pub build_roots: Vec<BuildRoot>,
    pub duplicates: Vec<DuplicateCluster>,
    pub totals: CategoryStats,
    /// True when `max_duration` cut the walk short, so figures are partial.
    pub truncated: bool,
    pub errors: u64,
    pub backend: String,
    /// Whether `extents` in every `Stats` is read from the filesystem (true) or estimated.
    pub extents_exact: bool,
    pub dirs_listed: u64,
    pub dirs_cached: u64,
    pub notes: Vec<String>,
    pub walk_ms: u128,
    pub total_ms: u128,
}

fn special_category(
    path: &Path,
    dirs: &[(PathBuf, DirInfo)],
    index: &HashMap<&Path, usize>,
) -> Option<Category> {
    let name = path.file_name()?.to_str()?;
    if name == ".git" {
        return Some(Category::Git);
    }
    let parent = &dirs[*index.get(path.parent()?)?].1;
    let facts = ParentFacts {
        has_package_json: parent.has_package_json,
        has_cargo_toml: parent.has_cargo_toml,
    };
    match classify_dir(OsStr::new(name), facts)? {
        BuildKind::NodeModules => Some(Category::NodeModules),
        BuildKind::CargoTarget => Some(Category::CargoTarget),
    }
}

/// `gitdir: <main>/.git/worktrees/<name>` in a `.git` file marks a linked worktree.
fn worktree_main_repo(git_file: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(git_file).ok()?;
    let gitdir = Path::new(text.trim().strip_prefix("gitdir:")?.trim());
    let worktrees = gitdir.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    Some(worktrees.parent()?.parent()?.to_path_buf())
}

fn lockfile_hash(build_dir: &Path, kind: BuildKind) -> Option<u64> {
    let names: &[&str] = match kind {
        BuildKind::NodeModules => &[
            "package-lock.json",
            "npm-shrinkwrap.json",
            "yarn.lock",
            "pnpm-lock.yaml",
            "bun.lock",
            "bun.lockb",
        ],
        BuildKind::CargoTarget => &["Cargo.lock"],
    };
    let parent = build_dir.parent()?;
    for name in names {
        let path = parent.join(name);
        let Ok(md) = std::fs::metadata(&path) else {
            continue;
        };
        if md.len() > MAX_LOCKFILE_BYTES {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&path) {
            let mut h = DefaultHasher::new();
            bytes.hash(&mut h);
            return Some(h.finish());
        }
    }
    None
}

fn age_secs(mtime: i64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    u64::try_from(now - mtime).unwrap_or(0)
}

/// Pure classification pass over the walk's per-directory map.
fn build_report(
    roots: &[PathBuf],
    mut dirs: Vec<(PathBuf, DirInfo)>,
    started: Instant,
    walk_ms: u128,
) -> ScanReport {
    // Path ordering is component-wise, so every directory's descendants follow it contiguously.
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    let index: HashMap<&Path, usize> = dirs
        .iter()
        .enumerate()
        .map(|(i, (p, _))| (p.as_path(), i))
        .collect();

    let mut cats = vec![Category::Source; dirs.len()];
    let mut build_idx = Vec::new();
    let mut git_roots: HashMap<PathBuf, (GroupKind, PathBuf)> = HashMap::new();
    let mut active: Option<(&Path, Category)> = None;
    for (i, (path, info)) in dirs.iter().enumerate() {
        if active.is_some_and(|(root, _)| !path.starts_with(root)) {
            active = None;
        }
        if active.is_none() {
            if let Some(cat) = special_category(path, &dirs, &index) {
                active = Some((path, cat));
                match cat {
                    Category::Git => {
                        if let Some(parent) = path.parent() {
                            git_roots
                                .entry(parent.to_path_buf())
                                .or_insert((GroupKind::Checkout, parent.to_path_buf()));
                        }
                    }
                    _ => build_idx.push(i),
                }
            }
        }
        cats[i] = active.map_or(Category::Source, |(_, c)| c);
        if info.git_file && active.is_none() {
            let main = worktree_main_repo(&path.join(".git"));
            let entry = main.map_or((GroupKind::Checkout, path.clone()), |m| {
                (GroupKind::Worktree, m)
            });
            git_roots.insert(path.clone(), entry);
        }
    }

    let group_of = |path: &Path| -> (PathBuf, GroupKind, PathBuf) {
        for anc in path.ancestors() {
            if let Some((kind, repo)) = git_roots.get(anc) {
                return (anc.to_path_buf(), *kind, repo.clone());
            }
        }
        let loose = roots.iter().find(|r| path.starts_with(r)).map_or_else(
            || path.to_path_buf(),
            |r| {
                let rel = path.strip_prefix(r).unwrap_or(path);
                rel.components()
                    .next()
                    .map_or_else(|| r.clone(), |c| r.join(c))
            },
        );
        (loose.clone(), GroupKind::Loose, loose)
    };

    let mut groups: BTreeMap<PathBuf, Group> = BTreeMap::new();
    let mut totals = CategoryStats::default();
    for (i, (path, info)) in dirs.iter().enumerate() {
        // Files belong to the directory key, dir entries to their own path: same ancestry walk.
        let (gpath, kind, repo) = group_of(path);
        let g = groups.entry(gpath.clone()).or_insert_with(|| Group {
            path: gpath,
            kind,
            repo,
            stats: CategoryStats::default(),
            inodes: 0,
            metadata_est_bytes: 0,
        });
        g.stats.get_mut(cats[i]).merge(&info.stats);
        totals.get_mut(cats[i]).merge(&info.stats);
    }
    for g in groups.values_mut() {
        g.inodes = g.stats.combined().inodes;
        g.metadata_est_bytes = g.stats.metadata_est_bytes();
    }

    // Build roots: sum the subtree range that follows each root in sorted order.
    let mut build_roots = Vec::new();
    for &i in &build_idx {
        let (root, info) = &dirs[i];
        let mut stats = Stats::default();
        for (p, d) in &dirs[i..] {
            if !p.starts_with(root) {
                break;
            }
            stats.merge(&d.stats);
        }
        let kind = if cats[i] == Category::NodeModules {
            BuildKind::NodeModules
        } else {
            BuildKind::CargoTarget
        };
        build_roots.push(BuildRoot {
            path: root.clone(),
            kind,
            group: group_of(root).0,
            stats,
            metadata_est_bytes: stats.metadata_cost().total(),
            age_secs: age_secs(info.mtime),
            lockfile_hash: lockfile_hash(root, kind),
        });
    }

    let mut by_lock: HashMap<(BuildKind, u64), Vec<&BuildRoot>> = HashMap::new();
    for b in &build_roots {
        if let Some(h) = b.lockfile_hash {
            by_lock.entry((b.kind, h)).or_default().push(b);
        }
    }
    let mut duplicates: Vec<DuplicateCluster> = by_lock
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|((kind, lockfile_hash), v)| {
            let inodes: Vec<u64> = v.iter().map(|b| b.stats.inodes).collect();
            let total_inodes: u64 = inodes.iter().sum();
            let max_inodes = inodes.iter().copied().max().unwrap_or(0);
            let meta_total: u64 = v.iter().map(|b| b.metadata_est_bytes).sum();
            let meta_max = v.iter().map(|b| b.metadata_est_bytes).max().unwrap_or(0);
            let mut members: Vec<PathBuf> = v.iter().map(|b| b.path.clone()).collect();
            members.sort();
            DuplicateCluster {
                kind,
                lockfile_hash,
                members,
                min_inodes: inodes.iter().copied().min().unwrap_or(0),
                max_inodes,
                total_inodes,
                redundant_inodes: total_inodes - max_inodes,
                redundant_metadata_est_bytes: meta_total - meta_max,
            }
        })
        .collect();
    duplicates.sort_by_key(|d| std::cmp::Reverse(d.redundant_inodes));
    build_roots.sort_by_key(|b| std::cmp::Reverse(b.stats.inodes));

    let mut families: BTreeMap<PathBuf, Family> = BTreeMap::new();
    for g in groups.values() {
        let f = families.entry(g.repo.clone()).or_insert_with(|| Family {
            repo: g.repo.clone(),
            checkouts: 0,
            stats: CategoryStats::default(),
            inodes: 0,
            metadata_est_bytes: 0,
        });
        f.checkouts += 1;
        f.stats.merge(&g.stats);
        f.inodes += g.inodes;
        f.metadata_est_bytes += g.metadata_est_bytes;
    }

    ScanReport {
        roots: roots.to_vec(),
        groups: groups.into_values().collect(),
        families: families.into_values().collect(),
        build_roots,
        duplicates,
        totals,
        truncated: false,
        errors: 0,
        backend: String::new(),
        extents_exact: false,
        dirs_listed: 0,
        dirs_cached: 0,
        notes: Vec::new(),
        walk_ms,
        total_ms: started.elapsed().as_millis(),
    }
}

/// Drops roots nested inside another root so nothing is counted twice.
fn dedupe_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut sorted = roots.to_vec();
    sorted.sort();
    sorted.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for r in sorted {
        if !out.iter().any(|kept| r.starts_with(kept)) {
            out.push(r);
        }
    }
    out
}

pub fn scan(opts: &ScanOptions) -> Result<ScanReport> {
    let mut roots = Vec::new();
    for r in &opts.roots {
        roots.push(std::fs::canonicalize(r).map_err(|source| Error::Io {
            path: r.clone(),
            source,
        })?);
    }
    let roots = dedupe_roots(&roots);
    if roots.is_empty() {
        return Err(Error::Other("no scan roots given".into()));
    }

    let started = Instant::now();
    let threads = if opts.threads == 0 {
        (std::thread::available_parallelism().map_or(4, usize::from) * 3).min(64)
    } else {
        opts.threads
    };
    let req = CollectRequest {
        roots: &roots,
        cross_fs: opts.cross_fs,
        deadline: opts.max_duration.map(|d| started + d),
        threads,
    };
    let walker = WalkerBackend {
        cache: opts.cache.clone(),
    };
    let backend: &dyn Backend = match opts.backend {
        BackendChoice::Walker => &walker,
        BackendChoice::BtrfsTree => &BtrfsTreeBackend,
        BackendChoice::Auto if rustix::process::geteuid().is_root() => &BtrfsTreeBackend,
        BackendChoice::Auto => &walker,
    };
    let collected = backend.collect(&req)?;
    let walk_ms = started.elapsed().as_millis();

    let mut report = build_report(&roots, collected.dirs, started, walk_ms);
    report.truncated = collected.truncated;
    report.errors = collected.errors;
    report.backend = backend.name().to_string();
    report.extents_exact = backend.extents_exact();
    report.dirs_listed = collected.dirs_listed;
    report.dirs_cached = collected.dirs_cached;
    report.notes = collected.notes;
    Ok(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "disk-cleanup-scan-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::canonicalize(&d).unwrap()
    }

    fn opts(root: &Path) -> ScanOptions {
        ScanOptions {
            roots: vec![root.to_path_buf()],
            backend: BackendChoice::Walker,
            cross_fs: false,
            max_duration: None,
            threads: 2,
            cache: None,
        }
    }

    /// repo/ (main checkout with node_modules + target) and two linked worktrees with identical
    /// lockfiles, plus a loose dir.
    fn fixture(root: &Path) {
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".git/worktrees")).unwrap();
        fs::write(repo.join("package.json"), "{}").unwrap();
        fs::write(repo.join("package-lock.json"), "lock-v1").unwrap();
        fs::write(repo.join("Cargo.toml"), "").unwrap();
        fs::create_dir_all(repo.join("node_modules/dep/lib")).unwrap();
        fs::write(repo.join("node_modules/dep/lib/a.js"), "1").unwrap();
        fs::create_dir_all(repo.join("target/debug")).unwrap();
        fs::write(repo.join("target/debug/bin"), "x").unwrap();
        fs::write(repo.join("src.rs"), "fn main(){}").unwrap();
        for wt in ["wt1", "wt2"] {
            let dir = repo.join(".claude/worktrees").join(wt);
            fs::create_dir_all(dir.join("node_modules/dep/lib")).unwrap();
            fs::write(dir.join("package.json"), "{}").unwrap();
            fs::write(dir.join("package-lock.json"), "lock-v1").unwrap();
            fs::write(dir.join("node_modules/dep/lib/a.js"), "1").unwrap();
            fs::write(
                dir.join(".git"),
                format!("gitdir: {}/.git/worktrees/{wt}\n", repo.display()),
            )
            .unwrap();
        }
        fs::create_dir_all(root.join("loose/sub")).unwrap();
        fs::write(root.join("loose/sub/f"), "y").unwrap();
    }

    #[test]
    fn attributes_build_dirs_worktrees_and_duplicates() {
        let root = tmp("attr");
        fixture(&root);
        let r = scan(&opts(&root)).unwrap();
        let _ = fs::remove_dir_all(&root);

        assert!(!r.truncated && r.errors == 0);
        let repo = root.join("repo");
        let fam = r.families.iter().find(|f| f.repo == repo).unwrap();
        assert_eq!(fam.checkouts, 3, "main + 2 worktrees");
        assert!(r.families.iter().any(|f| f.repo == root.join("loose")));

        // node_modules/{dep,dep/lib,a.js} plus node_modules itself = 4 inodes per copy.
        let nm: Vec<_> = r
            .build_roots
            .iter()
            .filter(|b| b.kind == BuildKind::NodeModules)
            .collect();
        assert_eq!(nm.len(), 3);
        assert!(nm.iter().all(|b| b.stats.inodes == 4));
        let tgt = r
            .build_roots
            .iter()
            .find(|b| b.kind == BuildKind::CargoTarget)
            .unwrap();
        assert_eq!(tgt.stats.inodes, 3); // target, target/debug, bin

        let dup = &r.duplicates[0];
        assert_eq!(
            (dup.kind, dup.members.len(), dup.redundant_inodes),
            (BuildKind::NodeModules, 3, 8)
        );

        // Worktrees nested under the repo path are not counted as the main checkout's source.
        let main = r.groups.iter().find(|g| g.path == repo).unwrap();
        assert_eq!(main.kind, GroupKind::Checkout);
        assert_eq!(main.stats.node_modules.inodes, 4);
        assert_eq!(
            r.groups
                .iter()
                .filter(|g| g.kind == GroupKind::Worktree)
                .count(),
            2
        );
        assert!(r.totals.git.inodes >= 2);
    }

    #[test]
    fn marker_sibling_is_required() {
        let root = tmp("marker");
        fs::create_dir_all(root.join("a/node_modules/x")).unwrap(); // no package.json
        fs::create_dir_all(root.join("b/target/x")).unwrap(); // no Cargo.toml
        let r = scan(&opts(&root)).unwrap();
        let _ = fs::remove_dir_all(&root);
        assert!(r.build_roots.is_empty());
        assert_eq!(
            r.totals.node_modules.inodes + r.totals.cargo_target.inodes,
            0
        );
    }

    #[test]
    fn hard_links_count_one_inode() {
        let root = tmp("links");
        fs::write(root.join("a"), "data").unwrap();
        fs::hard_link(root.join("a"), root.join("b")).unwrap();
        let r = scan(&opts(&root)).unwrap();
        let _ = fs::remove_dir_all(&root);
        let all = r.totals.combined();
        assert_eq!((all.files, all.dir_entries), (1, 2)); // the scan root has no parent entry
    }

    #[test]
    fn nested_roots_are_deduped() {
        let a = PathBuf::from("/x/y");
        let kept = dedupe_roots(&[PathBuf::from("/x/y/z"), a.clone(), a.clone()]);
        assert_eq!(kept, vec![a]);
    }

    #[test]
    fn worktree_pointer_parsing() {
        let root = tmp("ptr");
        let f = root.join(".git");
        fs::write(&f, "gitdir: /r/repo/.git/worktrees/w1\n").unwrap();
        assert_eq!(worktree_main_repo(&f), Some(PathBuf::from("/r/repo")));
        fs::write(&f, "gitdir: /r/repo/.git/modules/sub\n").unwrap();
        assert_eq!(worktree_main_repo(&f), None);
        let _ = fs::remove_dir_all(&root);
    }
}

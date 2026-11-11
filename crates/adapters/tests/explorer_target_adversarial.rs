//! Blind adversarial cases for `AllowedTarget::BelowScanRoot` through the real engine (Task 6.6.5b).
//! Written from ADR-003 (amended 2026-11-11), ADR-006 A1 and the frozen contract only; fixtures live in
//! tempdirs, never in the real HOME.
//!
//! A witness comes only from `decide_path_common`, so each case obtains an `Allowed` under a permissive
//! grant (decoy home, one grant root) and builds the `DeletePolicy` with the home and scan roots under test.
//! Every refusal compares a manifest of the whole fixture before and after.
#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use disk_cleanup_adapters::{delete_tree, open_parent, open_verified_root, DirOps, RealDirOps};
use disk_cleanup_core::guard::{decide_path_common, Facts, GuardDecision};
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::{
    AbortFlag, AllocatedBytes, Allowed, ApparentBytes, ApplyToken, DeleteError, DeleteFailure,
    DeletePolicy, Dirs, Freed, Identity, Kind, RootDev,
};

// ---------------------------------------------------------------- fixtures

struct Fx {
    _td: tempfile::TempDir,
    base: PathBuf,
}

impl Fx {
    /// `base/hhh` is HOME, `base/work` is a non-home area, `base/gh` is the decoy grant home.
    fn new() -> Fx {
        let td = tempfile::tempdir().expect("tempdir");
        let base = fs::canonicalize(td.path()).expect("canonical");
        for d in ["hhh/code", "work", "gh"] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        Fx { _td: td, base }
    }
    fn p(&self, rel: &str) -> PathBuf {
        self.base.join(rel)
    }
}

fn write(path: &Path, bytes: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; bytes]).unwrap();
}

/// A directory with files, subdirectories, a symlink and a hardlink pair.
fn fill(dir: &Path) {
    fs::create_dir_all(dir.join("sub/deeper")).unwrap();
    write(&dir.join("a.txt"), 5000);
    write(&dir.join("sub/b.txt"), 12_000);
    write(&dir.join("sub/deeper/c.bin"), 1);
    write(&dir.join("sub/deeper/empty"), 0);
    fs::hard_link(dir.join("a.txt"), dir.join("sub/a-link.txt")).unwrap();
    symlink("a.txt", dir.join("sub/ln")).unwrap();
}

/// What `delete_tree` must report for a tree whose every name lies inside it (Glossary "Counting rule").
fn expected_freed(dir: &Path) -> Freed {
    fn walk(p: &Path, seen: &mut BTreeSet<(u64, u64)>, f: &mut Freed) {
        let m = fs::symlink_metadata(p).unwrap();
        f.entries_removed += 1;
        if seen.insert((m.dev(), m.ino())) {
            f.inodes_freed += 1;
            f.attributed_bytes.0 += m.blocks() * 512;
        }
        if m.is_dir() {
            for e in fs::read_dir(p).unwrap() {
                walk(&e.unwrap().path(), seen, f);
            }
        } else {
            f.apparent_bytes.0 += m.size();
        }
    }
    let mut f = Freed {
        attributed_bytes: AllocatedBytes(0),
        apparent_bytes: ApparentBytes(0),
        entries_removed: 0,
        inodes_freed: 0,
    };
    walk(dir, &mut BTreeSet::new(), &mut f);
    f
}

/// Path -> description for everything under `root` (lstat, content, symlink target). Byte-identical trees
/// have equal manifests.
fn manifest(root: &Path) -> BTreeMap<PathBuf, String> {
    fn walk(p: &Path, out: &mut BTreeMap<PathBuf, String>) {
        let m = fs::symlink_metadata(p).unwrap();
        let ft = m.file_type();
        let body = if ft.is_symlink() {
            format!("-> {}", fs::read_link(p).unwrap().display())
        } else if ft.is_file() {
            format!(
                "{:x}",
                fs::read(p)
                    .unwrap()
                    .iter()
                    .fold(0u64, |h, b| h.wrapping_mul(131).wrapping_add(u64::from(*b)))
            )
        } else {
            String::new()
        };
        out.insert(
            p.to_path_buf(),
            format!(
                "ino={} mode={:o} size={} nlink={} mtime={}.{} {body}",
                m.ino(),
                m.mode(),
                if ft.is_dir() { 0 } else { m.size() },
                m.nlink(),
                m.mtime(),
                m.mtime_nsec()
            ),
        );
        if ft.is_dir() {
            for e in fs::read_dir(p).unwrap() {
                walk(&e.unwrap().path(), out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out);
    out
}

fn real_canon(p: &Path) -> io::Result<(PathBuf, Identity)> {
    let c = fs::canonicalize(p)?;
    let m = fs::metadata(&c)?;
    Ok((c, Identity::of(&m)))
}

fn ident(p: &Path) -> Identity {
    Identity::of(&fs::symlink_metadata(p).unwrap())
}

fn facts_of(kind: Kind, path: &Path) -> Facts {
    let m = fs::symlink_metadata(path).unwrap();
    Facts {
        kind,
        path: path.to_path_buf(),
        ident: Identity::of(&m),
        is_symlink: false,
        root_dev: RootDev(m.dev()),
        is_mount_point: false,
    }
}

/// Obtains the witness for `target` under `grant_root` with the decoy home `<base>/gh`.
fn witness_under(fx: &Fx, grant_root: &Path, target: &Path) -> Allowed {
    let dirs = Dirs {
        home: fx.p("gh"),
        state: fx.p("gh/.local/state"),
        cache: fx.p("gh/.cache"),
        config: fx.p("gh/.config"),
    };
    let roots = RootSet::resolve(
        &dirs,
        &[grant_root.to_path_buf()],
        None,
        &fx.base,
        &real_canon,
    )
    .expect("grant roots resolve");
    match decide_path_common(&facts_of(Kind::ExplorerSelection, target), &roots) {
        GuardDecision::Allow(a) => a,
        GuardDecision::Skip(r) => panic!("the guard should allow {}: {r:?}", target.display()),
    }
}

struct Attempt<'a> {
    fx: &'a Fx,
    /// the directory the witness is granted under
    grant_root: PathBuf,
    /// the path the witness is for
    witness_for: PathBuf,
    home: PathBuf,
    home_id: Identity,
    scan_roots: Vec<PathBuf>,
    /// the engine's verified root; `rel_parent` and `name` select the deletion
    open_base: PathBuf,
    delete: PathBuf,
}

impl<'a> Attempt<'a> {
    /// Deleting `target` with a witness for `target`, a policy under `home`/`scan_roots`.
    fn new(
        fx: &'a Fx,
        grant_root: &Path,
        target: &Path,
        home: &Path,
        scan_roots: &[&Path],
    ) -> Self {
        Attempt {
            fx,
            grant_root: grant_root.to_path_buf(),
            witness_for: target.to_path_buf(),
            home: home.to_path_buf(),
            home_id: ident(home),
            scan_roots: scan_roots.iter().map(|p| p.to_path_buf()).collect(),
            open_base: target.parent().unwrap().to_path_buf(),
            delete: target.to_path_buf(),
        }
    }
    fn open_at(mut self, base: &Path) -> Self {
        self.open_base = base.to_path_buf();
        self
    }
    fn delete_other(mut self, other: &Path) -> Self {
        self.delete = other.to_path_buf();
        self
    }
    fn run(&self) -> Result<Freed, DeleteFailure> {
        self.run_with(|_| {})
    }
    fn run_with(&self, tamper: impl FnOnce(&mut DeletePolicy)) -> Result<Freed, DeleteFailure> {
        let token = ApplyToken::for_tests();
        let ops = RealDirOps::confined_to(&self.fx.base);
        let root =
            open_verified_root(&ops, &self.open_base, ident(&self.open_base)).expect("open root");
        let st = ops.fstat(root.dir()).expect("fstat root");
        let witness = witness_under(self.fx, &self.grant_root, &self.witness_for);
        let mut policy = DeletePolicy::for_allowed(
            &witness,
            RootDev(st.ident.dev),
            st.mnt_id.expect("kernel reports mnt_id"),
            self.home.clone(),
            self.home_id,
            self.scan_roots.clone(),
        )
        .expect("an explorer witness makes a policy");
        tamper(&mut policy);
        let rel_parent = self
            .delete
            .parent()
            .unwrap()
            .strip_prefix(&self.open_base)
            .unwrap();
        let parent = open_parent(&ops, &root, rel_parent).expect("open parent");
        let name = self.delete.file_name().unwrap();
        delete_tree(
            &ops,
            &parent,
            name,
            ident(&self.delete),
            &policy,
            &AbortFlag::new(),
            &token,
        )
    }
}

fn assert_refused_intact(fx: &Fx, a: &Attempt<'_>, what: &str) {
    let before = manifest(&fx.base);
    let r = a.run();
    let after = manifest(&fx.base);
    match r {
        Err(DeleteFailure { error, freed }) => {
            assert!(
                matches!(
                    error,
                    DeleteError::OutsideScope | DeleteError::ProtectedPath
                ),
                "{what}: expected OutsideScope or ProtectedPath, got {error:?}"
            );
            assert_eq!(freed, Freed::default(), "{what}: a refusal frees nothing");
        }
        Ok(f) => panic!("{what}: the engine deleted {f:?}"),
    }
    assert_eq!(before, after, "{what}: the tree changed");
}

fn assert_deleted_with_accounting(a: &Attempt<'_>, target: &Path) {
    fill(target);
    let expect = expected_freed(target);
    let sibling = target.parent().unwrap().join("keep-me");
    write(&sibling.join("k.txt"), 100);
    let sib_before = manifest(&sibling);
    let freed = a
        .run()
        .unwrap_or_else(|e| panic!("{}: {e:?}", target.display()));
    assert!(
        !target.exists() && fs::symlink_metadata(target).is_err(),
        "target must be gone"
    );
    assert_eq!(freed, expect, "Freed accounting");
    assert!(freed.entries_removed >= 7 && freed.inodes_freed >= 6);
    assert_eq!(
        manifest(&sibling),
        sib_before,
        "the sibling directory must be untouched"
    );
}

fn dirs_under(fx: &Fx, rel: &str) -> (PathBuf, PathBuf) {
    let root = fx.p(rel);
    fs::create_dir_all(&root).unwrap();
    (root.clone(), root)
}

// ---------------------------------------------------------------- accepted cases

#[test]
fn below_scan_root_should_delete_and_count_When_the_directory_is_one_level_below_a_root() {
    let fx = Fx::new();
    let (root, _) = dirs_under(&fx, "work");
    let t = root.join("a");
    let a = Attempt::new(&fx, &root, &t, &fx.p("hhh"), &[&root]);
    fill(&t);
    let expect = expected_freed(&t);
    let freed = a
        .run()
        .expect("depth 1 below a root outside HOME is deletable");
    assert_eq!(freed, expect);
    assert!(fs::symlink_metadata(&t).is_err());
    assert!(root.is_dir(), "the scan root stays");
}

#[test]
fn below_scan_root_should_delete_and_count_When_the_directory_is_two_levels_below_a_root() {
    let fx = Fx::new();
    let root = fx.p("hhh/code");
    let t = root.join("p/q");
    fs::create_dir_all(&t).unwrap();
    let a = Attempt::new(&fx, &root, &t, &fx.p("hhh"), &[&root]).open_at(&root);
    assert_deleted_with_accounting(&a, &t);
    assert!(
        root.join("p").is_dir(),
        "only the target goes, not its parent"
    );
}

#[test]
fn below_scan_root_should_delete_and_count_When_the_directory_is_six_levels_below_a_root() {
    let fx = Fx::new();
    let root = fx.p("hhh/code");
    let t = root.join("a/b/c/d/e/f");
    fs::create_dir_all(&t).unwrap();
    let a = Attempt::new(&fx, &root, &t, &fx.p("hhh"), &[&root]).open_at(&root);
    assert_deleted_with_accounting(&a, &t);
    assert!(root.join("a/b/c/d/e").is_dir());
}

#[test]
fn below_scan_root_should_delete_When_the_depth_below_home_is_exactly_two() {
    let fx = Fx::new();
    let root = fx.p("hhh/code");
    let t = root.join("app");
    let a = Attempt::new(&fx, &root, &t, &fx.p("hhh"), &[&root]);
    assert_deleted_with_accounting(&a, &t);
}

#[test]
fn below_scan_root_should_delete_lookalike_git_names_When_no_component_is_dot_git() {
    for name in [".gitx", "my.git", ".git-old", "git", ".github"] {
        let fx = Fx::new();
        let root = fx.p("hhh/code");
        let t = root.join(name);
        let a = Attempt::new(&fx, &root, &t, &fx.p("hhh"), &[&root]);
        assert_deleted_with_accounting(&a, &t);
    }
    // a lookalike as an ancestor component
    let fx = Fx::new();
    let root = fx.p("hhh/code");
    let t = root.join(".gitx/my.git/inner");
    fs::create_dir_all(&t).unwrap();
    let a = Attempt::new(&fx, &root, &t, &fx.p("hhh"), &[&root]).open_at(&root);
    assert_deleted_with_accounting(&a, &t);
}

#[test]
fn below_scan_root_should_delete_When_roots_are_nested_in_either_list_order() {
    for order in [0, 1] {
        let fx = Fx::new();
        let outer = fx.p("hhh/code");
        let inner = outer.join("app");
        fs::create_dir_all(&inner).unwrap();
        let t = inner.join("build");
        let roots: Vec<&Path> = if order == 0 {
            vec![&outer, &inner]
        } else {
            vec![&inner, &outer]
        };
        let a = Attempt::new(&fx, &inner, &t, &fx.p("hhh"), &roots);
        assert_deleted_with_accounting(&a, &t);
        assert!(inner.is_dir() && outer.is_dir());
    }
}

// ---------------------------------------------------------------- refusals: the tree stays byte-identical

fn precious_fixture(fx: &Fx) -> PathBuf {
    let t = fx.p("hhh/code/app");
    fill(&t);
    write(&fx.p("hhh/.ssh/id_ed25519"), 400);
    write(&fx.p("hhh/Documents/thesis.txt"), 4000);
    write(&fx.p("hhh/code/other/keep.txt"), 10);
    write(&fx.p("work/w.txt"), 10);
    t
}

#[test]
fn safety_below_scan_root_should_refuse_and_keep_the_tree_When_the_target_is_the_scan_root() {
    for order in [0, 1] {
        let fx = Fx::new();
        let t = precious_fixture(&fx);
        let outer = fx.p("hhh/code");
        let roots: Vec<&Path> = if order == 0 {
            vec![&outer, &t]
        } else {
            vec![&t, &outer]
        };
        let a = Attempt::new(&fx, &outer, &t, &fx.p("hhh"), &roots);
        assert_refused_intact(&fx, &a, "scan root as target, nested roots");
    }
    let fx = Fx::new();
    let t = precious_fixture(&fx);
    let a = Attempt::new(&fx, &fx.p("hhh/code"), &t, &fx.p("hhh"), &[&t]);
    assert_refused_intact(&fx, &a, "scan root as the only root");
}

#[test]
fn safety_below_scan_root_should_refuse_and_keep_the_tree_When_the_target_is_beside_the_roots() {
    let fx = Fx::new();
    let t = precious_fixture(&fx);
    let grant_root = fx.p("hhh/code");
    for roots in [vec![fx.p("hhh/other")], vec![fx.p("work")], vec![]] {
        let refs: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
        let a = Attempt::new(&fx, &grant_root, &t, &fx.p("hhh"), &refs);
        assert_refused_intact(&fx, &a, &format!("roots {roots:?}"));
    }
}

#[test]
fn safety_below_scan_root_should_refuse_and_keep_the_tree_When_a_root_is_a_string_prefix_lookalike()
{
    let fx = Fx::new();
    let t = precious_fixture(&fx);
    let grant_root = fx.p("hhh/code");
    for lookalike in ["hhh/cod", "hhh/code/ap"] {
        fs::create_dir_all(fx.p(lookalike)).unwrap();
        let r = fx.p(lookalike);
        let a = Attempt::new(&fx, &grant_root, &t, &fx.p("hhh"), &[&r]);
        assert_refused_intact(&fx, &a, lookalike);
    }
}

#[test]
fn safety_below_scan_root_should_refuse_and_keep_the_tree_When_the_target_is_home() {
    let fx = Fx::new();
    let t = precious_fixture(&fx);
    let root = fx.p("hhh/code");
    let a = Attempt::new(&fx, &root, &t, &t, &[&root]);
    assert_refused_intact(&fx, &a, "target == HOME");
}

#[test]
fn safety_below_scan_root_should_refuse_and_keep_the_tree_When_home_is_inside_the_target() {
    let fx = Fx::new();
    let t = precious_fixture(&fx);
    let home = t.join("sub/deeper");
    let root = fx.p("hhh/code");
    let a = Attempt::new(&fx, &root, &t, &home, &[&root]);
    assert_refused_intact(&fx, &a, "target is an ancestor of HOME");
    let a = Attempt::new(&fx, &root, &t, &t.join("sub"), &[&root]);
    assert_refused_intact(&fx, &a, "target is the parent of HOME");
}

#[test]
fn safety_below_scan_root_should_refuse_and_keep_the_tree_When_the_target_is_home_by_identity() {
    let fx = Fx::new();
    let t = precious_fixture(&fx);
    let root = fx.p("hhh/code");
    let mut a = Attempt::new(&fx, &root, &t, &fx.p("hhh"), &[&root]);
    a.home = fx.p("a-different-spelling");
    a.home_id = ident(&t);
    assert_refused_intact(&fx, &a, "home id equals the target's identity");
}

#[test]
fn safety_below_scan_root_should_refuse_with_protected_path_and_keep_the_tree_When_one_level_below_home(
) {
    for name in ["code", ".ssh", "Documents"] {
        let fx = Fx::new();
        let _ = precious_fixture(&fx);
        let home = fx.p("hhh");
        let t = home.join(name);
        let grant_root = home.clone();
        // witness under the decoy home grant; the policy's root is HOME itself
        let a = Attempt::new(&fx, &grant_root, &t, &home, &[&home]);
        let before = manifest(&fx.base);
        let r = a.run();
        assert_eq!(
            r.as_ref().err().map(|e| &e.error),
            Some(&DeleteError::ProtectedPath),
            "{name}: {r:?}"
        );
        assert_eq!(manifest(&fx.base), before, "{name}: the tree changed");
    }
}

#[test]
fn safety_below_scan_root_should_refuse_and_keep_the_tree_When_the_policy_target_names_another_directory(
) {
    let fx = Fx::new();
    let t = precious_fixture(&fx);
    let other = fx.p("hhh/code/other");
    let root = fx.p("hhh/code");
    // policy and witness are for `t`, the call deletes `other`
    let a = Attempt::new(&fx, &root, &t, &fx.p("hhh"), &[&root]).delete_other(&other);
    assert_refused_intact(&fx, &a, "witness for app, delete of other");
    // policy.target rewritten to `other` while the witness stays for `t`
    let a = Attempt::new(&fx, &root, &t, &fx.p("hhh"), &[&root]).delete_other(&other);
    let before = manifest(&fx.base);
    let r = a.run_with(|p| p.target = other.clone());
    assert!(
        r.is_err(),
        "rewriting the target must not widen the scope: {r:?}"
    );
    assert_eq!(manifest(&fx.base), before);
}

#[test]
fn safety_below_scan_root_should_refuse_and_keep_the_git_directory_When_the_policy_is_rewritten_to_it(
) {
    let fx = Fx::new();
    let t = precious_fixture(&fx);
    let git = t.join(".git");
    write(&git.join("objects/aa/bb"), 100);
    write(&git.join("HEAD"), 20);
    let root = fx.p("hhh/code");
    let a = Attempt::new(&fx, &root, &t, &fx.p("hhh"), &[&root])
        .open_at(&t)
        .delete_other(&git);
    let before = manifest(&fx.base);
    let r = a.run_with(|p| p.target = git.clone());
    assert!(r.is_err(), "a .git directory must not be deletable: {r:?}");
    assert_eq!(manifest(&fx.base), before);
}

#[test]
fn safety_below_scan_root_should_refuse_and_keep_the_tree_When_the_target_is_a_regular_file() {
    let fx = Fx::new();
    let _ = precious_fixture(&fx);
    let root = fx.p("hhh/code");
    let file = root.join("other/keep.txt");
    let a = Attempt::new(&fx, &root, &file, &fx.p("hhh"), &[&root]);
    let before = manifest(&fx.base);
    let r = a.run();
    assert!(r.is_err(), "the engine deletes directories only: {r:?}");
    assert_eq!(manifest(&fx.base), before);
    assert!(file.is_file());
}

#[test]
fn safety_below_scan_root_should_refuse_and_keep_the_tree_When_the_target_is_a_symlink_to_a_directory(
) {
    let fx = Fx::new();
    let _ = precious_fixture(&fx);
    let root = fx.p("hhh/code");
    let link = root.join("ln-to-docs");
    symlink(fx.p("hhh/Documents"), &link).unwrap();
    let a = Attempt::new(&fx, &root, &link, &fx.p("hhh"), &[&root]);
    let before = manifest(&fx.base);
    let r = a.run();
    assert!(
        r.is_err(),
        "a symlink target must not be followed or removed: {r:?}"
    );
    assert_eq!(manifest(&fx.base), before);
    assert!(fs::read(fx.p("hhh/Documents/thesis.txt")).is_ok());
}

#[test]
fn safety_below_scan_root_should_not_follow_a_symlink_inside_the_target_When_deleting() {
    let fx = Fx::new();
    let t = precious_fixture(&fx);
    symlink(fx.p("hhh/Documents"), t.join("sub/to-docs")).unwrap();
    symlink(fx.p("hhh/.ssh/id_ed25519"), t.join("to-key")).unwrap();
    let root = fx.p("hhh/code");
    let docs_before = manifest(&fx.p("hhh/Documents"));
    let ssh_before = manifest(&fx.p("hhh/.ssh"));
    let a = Attempt::new(&fx, &root, &t, &fx.p("hhh"), &[&root]);
    a.run()
        .expect("symlinks inside the tree are unlinked, not followed");
    assert!(fs::symlink_metadata(&t).is_err());
    assert_eq!(manifest(&fx.p("hhh/Documents")), docs_before);
    assert_eq!(manifest(&fx.p("hhh/.ssh")), ssh_before);
}

#[test]
fn safety_below_scan_root_should_not_depend_on_root_list_order_When_refusing_nested_roots() {
    let fx = Fx::new();
    let t = precious_fixture(&fx);
    let outer = fx.p("hhh/code");
    let sibling_root = fx.p("hhh/code/other");
    let mut results = Vec::new();
    for roots in [
        vec![outer.as_path(), t.as_path(), sibling_root.as_path()],
        vec![sibling_root.as_path(), t.as_path(), outer.as_path()],
        vec![t.as_path(), sibling_root.as_path(), outer.as_path()],
    ] {
        let a = Attempt::new(&fx, &outer, &t, &fx.p("hhh"), &roots);
        let before = manifest(&fx.base);
        let r = a.run().map_err(|e| e.error);
        assert_eq!(manifest(&fx.base), before);
        results.push(r);
    }
    assert!(
        results.iter().all(|r| r == &results[0]),
        "order changed the verdict: {results:?}"
    );
    assert!(results[0].is_err());
}

// ---------------------------------------------------------------- bind mount (unshare -rm)

const NS_DIR: &str = "DC_BIND_DIR";

fn bind_skip(test: &str, why: &str) -> bool {
    assert!(
        std::env::var_os("DC_REQUIRE_BIND_MOUNT").is_none(),
        "{test}: bind-mount leg required but skipped ({why})"
    );
    eprintln!("skip: {test}: {why}");
    false
}

/// Re-runs `child` of this binary inside `unshare -rm` with `<dir>/outside` bind-mounted onto `<dir>/<mount_at>`.
/// Returns false when the leg was skipped.
fn run_in_bind_ns(child: &str, dir: &Path, mount_at: &str) -> bool {
    let out = Command::new("unshare")
        .args(["-rm", "sh", "-c"])
        .arg("mount --bind \"$1/outside\" \"$1/$2\" && shift 2 && exec \"$@\"")
        .arg("sh")
        .arg(dir)
        .arg(mount_at)
        .arg(std::env::current_exe().expect("exe"))
        .args(["--exact", child, "--nocapture", "--test-threads=1"])
        .env(NS_DIR, dir)
        .output();
    let marker = format!("CHILD_OK:{child}");
    match out {
        Ok(o) if String::from_utf8_lossy(&o.stdout).contains(&marker) => true,
        Ok(o) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            assert!(
                !text.contains("running 1 test"),
                "child {child} failed:\n{text}"
            );
            bind_skip(child, "unshare -rm or mount --bind not permitted")
        }
        Err(_) => bind_skip(child, "unshare not installed"),
    }
}

fn bind_fixture() -> (tempfile::TempDir, PathBuf) {
    let td = tempfile::tempdir().expect("tempdir");
    let dir = fs::canonicalize(td.path()).unwrap();
    fs::create_dir_all(dir.join("hhh/code/app/mnt")).unwrap();
    fs::create_dir_all(dir.join("gh")).unwrap();
    fill(&dir.join("hhh/code/app"));
    fs::create_dir_all(dir.join("outside/deep")).unwrap();
    write(&dir.join("outside/precious.txt"), 3000);
    write(&dir.join("outside/deep/more.txt"), 3000);
    fs::set_permissions(dir.join("outside"), fs::Permissions::from_mode(0o755)).unwrap();
    (td, dir)
}

fn child_dir() -> Option<PathBuf> {
    std::env::var_os(NS_DIR).map(PathBuf::from)
}

#[test]
fn safety_below_scan_root_should_return_cross_device_and_keep_outside_data_When_a_bind_mount_is_below_the_target(
) {
    let (_td, dir) = bind_fixture();
    run_in_bind_ns(
        "bind_mount_below_child_should_report_cross_device_When_run_inside_the_namespace",
        &dir,
        "hhh/code/app/mnt",
    );
}

#[test]
fn bind_mount_below_child_should_report_cross_device_When_run_inside_the_namespace() {
    let Some(dir) = child_dir() else { return };
    let fx = Fx {
        _td: tempfile::tempdir().unwrap(),
        base: dir.clone(),
    };
    let t = dir.join("hhh/code/app");
    let root = dir.join("hhh/code");
    let outside_before = manifest(&dir.join("outside"));
    let a = Attempt::new(&fx, &root, &t, &dir.join("hhh"), &[&root]).open_at(&root);
    let r = a.run();
    match &r {
        Err(DeleteFailure {
            error: DeleteError::CrossDevice,
            ..
        }) => {}
        other => panic!("expected CrossDevice, got {other:?}"),
    }
    assert_eq!(
        manifest(&dir.join("outside")),
        outside_before,
        "the other side of the mount is intact"
    );
    assert_eq!(
        manifest(&t.join("mnt")),
        manifest(&dir.join("outside"))
            .into_iter()
            .map(|(k, v)| (
                t.join("mnt")
                    .join(k.strip_prefix(dir.join("outside")).unwrap()),
                v
            ))
            .collect(),
        "the mount point still shows the outside data"
    );
    println!(
        "CHILD_OK:bind_mount_below_child_should_report_cross_device_When_run_inside_the_namespace"
    );
}

#[test]
fn safety_below_scan_root_should_return_cross_device_and_keep_outside_data_When_the_target_is_a_bind_mount(
) {
    let (_td, dir) = bind_fixture();
    run_in_bind_ns(
        "bind_mount_target_child_should_report_cross_device_When_run_inside_the_namespace",
        &dir,
        "hhh/code/app/mnt",
    );
}

#[test]
fn bind_mount_target_child_should_report_cross_device_When_run_inside_the_namespace() {
    let Some(dir) = child_dir() else { return };
    let fx = Fx {
        _td: tempfile::tempdir().unwrap(),
        base: dir.clone(),
    };
    let t = dir.join("hhh/code/app/mnt");
    let root = dir.join("hhh/code");
    let before_all = manifest(&dir);
    let a = Attempt::new(&fx, &root, &t, &dir.join("hhh"), &[&root]).open_at(&root);
    let r = a.run();
    match &r {
        Err(DeleteFailure {
            error: DeleteError::CrossDevice,
            freed,
        }) => {
            assert_eq!(
                *freed,
                Freed::default(),
                "nothing may be counted or removed"
            );
        }
        other => panic!("expected CrossDevice for a bind-mounted target, got {other:?}"),
    }
    assert_eq!(
        manifest(&dir),
        before_all,
        "the tree, mount included, is byte-identical"
    );
    println!(
        "CHILD_OK:bind_mount_target_child_should_report_cross_device_When_run_inside_the_namespace"
    );
}

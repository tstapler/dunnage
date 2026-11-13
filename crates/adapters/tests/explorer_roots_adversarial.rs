//! Blind adversarial cases for the revised delete-engine contract of Task 6.6.3f, against real directories.
//! Written from ADR-003, ADR-006 (amended 2026-11-11), plan.md Task 6.6.3f and the frozen contract only;
//! fixtures live in tempdirs, never in the real HOME.
//!
//! Layout of a fixture: `<base>/hhh/mid/home/code` (`home` is `$HOME`; `mid` and `hhh` are its ancestors
//! below the fixture), `<base>/work` (a scan root outside HOME). The permissive grant that mints witnesses
//! uses a second tempdir as its decoy home, so the home chain under test is not the chain of the grant.
//! Every refusal compares a manifest of the whole fixture before and after.
#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use dunnage_adapters::{delete_tree, open_parent, open_verified_root, DirOps, RealDirOps};
use dunnage_core::delete_types::FileKind;
use dunnage_core::floor_rule::ForAllowedError;
use dunnage_core::guard::{decide_path_common, Facts, GuardDecision};
use dunnage_core::plan::ItemIdentity;
use dunnage_core::roots::RootSet;
use dunnage_core::{
    AbortFlag, Allowed, ApplyToken, DeleteError, DeleteFailure, DeletePolicy, Dirs, Freed,
    Identity, Kind, RootDev, SkipReason,
};

// ---------------------------------------------------------------- fixtures

struct Fx {
    _td: tempfile::TempDir,
    _decoy: tempfile::TempDir,
    base: PathBuf,
    decoy_home: PathBuf,
}

impl Fx {
    fn new() -> Fx {
        let td = tempfile::tempdir().expect("tempdir");
        let decoy = tempfile::tempdir().expect("decoy tempdir");
        let base = fs::canonicalize(td.path()).expect("canonical");
        let decoy_home = fs::canonicalize(decoy.path()).expect("canonical");
        for d in ["hhh/mid/home/code", "hhh/mid/sibling", "work"] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        Fx {
            _td: td,
            _decoy: decoy,
            base,
            decoy_home,
        }
    }
    fn p(&self, rel: &str) -> PathBuf {
        self.base.join(rel)
    }
    fn home(&self) -> PathBuf {
        self.p("hhh/mid/home")
    }
    /// The roots under test: `$HOME` is `hhh/mid/home`, scan roots `home/code` and `work`.
    fn roots(&self) -> RootSet {
        self.roots_with(&[self.p("hhh/mid/home/code"), self.p("work")])
    }
    fn roots_with(&self, scan: &[PathBuf]) -> RootSet {
        let h = self.home();
        let dirs = Dirs {
            home: h.clone(),
            state: h.join(".local/state"),
            cache: h.join(".cache"),
            config: h.join(".config"),
        };
        RootSet::resolve(&dirs, scan, None, &self.base, &real_canon).expect("roots resolve")
    }
    /// A witness for `target` from a grant that knows nothing about the home chain under test.
    fn witness(&self, kind: Kind, target: &Path, grant_root: &Path) -> Result<Allowed, String> {
        let d = &self.decoy_home;
        let dirs = Dirs {
            home: d.clone(),
            state: d.join(".local/state"),
            cache: d.join(".cache"),
            config: d.join(".config"),
        };
        let permissive = RootSet::resolve(
            &dirs,
            &[grant_root.to_path_buf()],
            None,
            &self.base,
            &real_canon,
        )
        .expect("grant roots resolve");
        match decide_path_common(&facts_of(kind, target), &permissive) {
            GuardDecision::Allow(a) => Ok(a),
            GuardDecision::Skip(r) => Err(format!("{r:?}")),
        }
    }
    fn explorer_witness(&self, target: &Path) -> Allowed {
        self.witness(Kind::ExplorerSelection, target, &self.base)
            .unwrap_or_else(|why| panic!("the grant should allow {}: {why}", target.display()))
    }
}

fn write(path: &Path, bytes: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; bytes]).unwrap();
}

fn fill(dir: &Path) {
    fs::create_dir_all(dir.join("sub/deeper")).unwrap();
    write(&dir.join("a.txt"), 5000);
    write(&dir.join("sub/b.txt"), 12_000);
    write(&dir.join("sub/deeper/c.bin"), 1);
    fs::hard_link(dir.join("a.txt"), dir.join("sub/a-link.txt")).unwrap();
    symlink("a.txt", dir.join("sub/ln")).unwrap();
}

/// Path -> description of everything under `root` (lstat, content hash, symlink target).
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
                "dev={} ino={} mode={:o} size={} nlink={} mtime={}.{} {body}",
                m.dev(),
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

fn dir_plan(p: &Path) -> ItemIdentity {
    let i = ident(p);
    ItemIdentity {
        dev: i.dev,
        ino: i.ino,
        kind: FileKind::Dir,
        btime: None,
    }
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

fn mnt_of(p: &Path) -> u64 {
    let ops = RealDirOps::confined_to(p);
    let root = open_verified_root(&ops, p, ident(p)).expect("open root");
    ops.fstat(root.dir())
        .expect("fstat")
        .mnt_id
        .expect("kernel reports mnt_id")
}

fn build(
    fx: &Fx,
    w: &Allowed,
    roots: &RootSet,
    planned: ItemIdentity,
) -> Result<DeletePolicy, ForAllowedError> {
    let dev = ident(&fx.base).dev;
    DeletePolicy::for_allowed_roots(w, RootDev(dev), mnt_of(&fx.base), roots, planned)
}

fn describe(r: &Result<DeletePolicy, ForAllowedError>) -> String {
    match r {
        Ok(_) => "Ok".into(),
        Err(e) => format!("{e:?}"),
    }
}

fn is_protected(r: &Result<DeletePolicy, ForAllowedError>) -> bool {
    matches!(r, Err(ForAllowedError::ProtectedPath))
}

/// Runs `delete_tree` for `target` (a child of `open_base`) with `expect`.
fn run_delete(
    fx: &Fx,
    policy: &DeletePolicy,
    target: &Path,
    expect: Identity,
) -> Result<Freed, DeleteFailure> {
    let open_base = target.parent().unwrap();
    let token = ApplyToken::for_tests();
    let ops = RealDirOps::confined_to(&fx.base);
    let root = open_verified_root(&ops, open_base, ident(open_base)).expect("open root");
    let parent = open_parent(&ops, &root, Path::new("")).expect("open parent");
    delete_tree(
        &ops,
        &parent,
        target.file_name().unwrap(),
        expect,
        policy,
        &AbortFlag::new(),
        &token,
    )
}

// ---------------------------------------------------------------- expect is tied to the witness

#[test]
fn delete_tree_should_delete_the_tree_When_expect_is_the_witness_identity() {
    let fx = Fx::new();
    let t = fx.p("work/app");
    fill(&t);
    let w = fx.explorer_witness(&t);
    let policy = build(&fx, &w, &fx.roots(), dir_plan(&t)).expect("policy");
    run_delete(&fx, &policy, &t, ident(&t)).expect("the planned directory is deleted");
    assert!(fs::symlink_metadata(&t).is_err(), "target must be gone");
    assert!(fx.p("work").is_dir());
}

#[test]
fn safety_delete_tree_should_refuse_identity_mismatch_When_expect_is_the_fresh_identity_of_a_replacement(
) {
    // The template trap (review 2 N4): the directory is replaced after the plan; the caller passes the
    // identity of what is there now as `expect`. The on-disk entry matches `expect`, so only the tie to the
    // witness identity can stop it.
    let fx = Fx::new();
    let t = fx.p("work/app");
    fill(&t);
    let w = fx.explorer_witness(&t);
    let planned = dir_plan(&t);
    // The original stays on disk under another name, so the replacement cannot reuse its inode.
    fs::rename(&t, fx.p("work/app.old")).unwrap();
    fs::create_dir(&t).unwrap();
    fill(&t);
    assert_ne!(
        ident(&t),
        Identity {
            dev: planned.dev,
            ino: planned.ino
        }
    );
    let policy = build(&fx, &w, &fx.roots(), planned).expect("the plan agrees with the witness");
    let before = manifest(&fx.base);
    let r = run_delete(&fx, &policy, &t, ident(&t));
    let after = manifest(&fx.base);
    match r {
        Err(DeleteFailure { error, freed }) => {
            assert_eq!(error, DeleteError::IdentityMismatch);
            assert_eq!(freed, Freed::default(), "a refusal frees nothing");
        }
        Ok(f) => panic!("the engine deleted a replacement it was never planned for: {f:?}"),
    }
    assert_eq!(before, after, "the tree changed");
}

#[test]
fn safety_delete_tree_should_refuse_identity_mismatch_When_expect_is_another_directorys_identity() {
    let fx = Fx::new();
    let t = fx.p("work/app");
    let other = fx.p("work/other");
    fill(&t);
    fill(&other);
    let w = fx.explorer_witness(&t);
    let policy = build(&fx, &w, &fx.roots(), dir_plan(&t)).expect("policy");
    let before = manifest(&fx.base);
    let r = run_delete(&fx, &policy, &t, ident(&other));
    let after = manifest(&fx.base);
    match r {
        Err(DeleteFailure { error, freed }) => {
            assert_eq!(error, DeleteError::IdentityMismatch);
            assert_eq!(freed, Freed::default());
        }
        Ok(f) => panic!("the engine deleted with a foreign expect: {f:?}"),
    }
    assert_eq!(before, after, "the tree changed");
}

#[test]
fn safety_delete_tree_should_refuse_identity_mismatch_When_the_policy_witness_was_made_for_a_sibling_with_the_same_parent(
) {
    // Witness for `a`, but the caller deletes `b` with b's own identity as `expect`.
    let fx = Fx::new();
    let (a, b) = (fx.p("work/a"), fx.p("work/b"));
    fill(&a);
    fill(&b);
    let w = fx.explorer_witness(&a);
    let policy = build(&fx, &w, &fx.roots(), dir_plan(&a)).expect("policy");
    let before = manifest(&fx.base);
    let r = run_delete(&fx, &policy, &b, ident(&b));
    let after = manifest(&fx.base);
    assert!(r.is_err(), "a policy made for a must not delete b");
    assert_eq!(before, after, "the tree changed");
}

// ---------------------------------------------------------------- the constructor refusals, real identities

#[test]
fn safety_for_allowed_roots_should_return_changed_since_plan_When_the_directory_was_replaced_after_the_plan(
) {
    let fx = Fx::new();
    let t = fx.p("work/app");
    fill(&t);
    let w = fx.explorer_witness(&t);
    let stale = dir_plan(&t);
    // The directory is replaced; a fresh witness describes the new one, the plan the old one.
    fs::rename(&t, fx.p("work/app.old")).unwrap();
    fs::create_dir(&t).unwrap();
    let w_new = fx.explorer_witness(&t);
    let before = manifest(&fx.base);
    let r = build(&fx, &w_new, &fx.roots(), stale);
    assert!(
        matches!(r, Err(ForAllowedError::ChangedSincePlan)),
        "{}",
        describe(&r)
    );
    assert_eq!(manifest(&fx.base), before);
    drop(w);
}

#[test]
fn safety_for_allowed_roots_should_return_changed_since_plan_When_only_the_device_or_only_the_inode_differs(
) {
    let fx = Fx::new();
    let t = fx.p("work/app");
    fill(&t);
    let w = fx.explorer_witness(&t);
    let good = dir_plan(&t);
    let mut dev = good;
    dev.dev = dev.dev.wrapping_add(1);
    let mut ino = good;
    ino.ino = ino.ino.wrapping_add(1);
    for (label, pl) in [("device", dev), ("inode", ino)] {
        let r = build(&fx, &w, &fx.roots(), pl);
        assert!(
            matches!(r, Err(ForAllowedError::ChangedSincePlan)),
            "{label}: {}",
            describe(&r)
        );
    }
    assert!(build(&fx, &w, &fx.roots(), good).is_ok(), "control");
}

#[test]
fn safety_for_allowed_roots_should_return_wrong_kind_When_the_witness_is_a_node_modules_witness() {
    let fx = Fx::new();
    let t = fx.p("hhh/mid/home/code/proj/node_modules");
    fs::create_dir_all(&t).unwrap();
    let w = fx
        .witness(Kind::NodeModules, &t, &fx.p("hhh/mid/home/code"))
        .expect("a node_modules witness is minted by the grant");
    let r = build(&fx, &w, &fx.roots(), dir_plan(&t));
    assert!(
        matches!(r, Err(ForAllowedError::WrongKind)),
        "{}",
        describe(&r)
    );
}

#[test]
fn safety_for_allowed_roots_should_return_not_a_directory_When_the_witness_is_a_file() {
    let fx = Fx::new();
    let f = fx.p("work/just-a-file");
    write(&f, 10);
    let w = fx.explorer_witness(&f);
    let i = ident(&f);
    let r = build(
        &fx,
        &w,
        &fx.roots(),
        ItemIdentity {
            dev: i.dev,
            ino: i.ino,
            kind: FileKind::File,
            btime: None,
        },
    );
    assert!(
        matches!(r, Err(ForAllowedError::NotADirectory)),
        "{}",
        describe(&r)
    );
    assert!(f.is_file(), "the file is untouched");
}

#[test]
fn safety_for_allowed_roots_should_return_protected_path_When_the_witness_is_home_an_ancestor_or_a_scan_root(
) {
    let fx = Fx::new();
    let before = manifest(&fx.base);
    let rs = fx.roots();
    let mut checked = 0;
    for (label, path) in [
        ("$HOME", fx.p("hhh/mid/home")),
        ("parent of $HOME", fx.p("hhh/mid")),
        ("grandparent of $HOME", fx.p("hhh")),
    ] {
        let w = fx.explorer_witness(&path);
        let r = build(&fx, &w, &rs, dir_plan(&path));
        assert!(is_protected(&r), "{label}: {}", describe(&r));
        checked += 1;
    }
    assert_eq!(checked, 3);
    assert_eq!(manifest(&fx.base), before, "refusals touch nothing");
}

#[test]
fn safety_for_allowed_roots_should_refuse_a_scan_root_at_construction_or_in_the_policy_When_it_is_the_witness(
) {
    let fx = Fx::new();
    for root in [fx.p("hhh/mid/home/code"), fx.p("work")] {
        let w = fx.explorer_witness(&root);
        let r = build(&fx, &w, &fx.roots(), dir_plan(&root));
        match r {
            Err(ForAllowedError::ProtectedPath) => {}
            Err(e) => panic!("{}: unexpected {e:?}", root.display()),
            Ok(p) => assert!(
                p.validate(root.file_name().unwrap()).is_err(),
                "{} is a scan root and must not validate",
                root.display()
            ),
        }
    }
}

#[test]
fn safety_for_allowed_roots_should_return_protected_path_for_home_and_ancestors_When_scan_root_order_changes(
) {
    let fx = Fx::new();
    let code = fx.p("hhh/mid/home/code");
    let work = fx.p("work");
    let extra = fx.p("hhh/mid/sibling");
    let all = [code, work, extra];
    for order in [
        [0usize, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let scan: Vec<PathBuf> = order.iter().map(|&i| all[i].clone()).collect();
        let rs = fx.roots_with(&scan);
        for path in [fx.p("hhh/mid/home"), fx.p("hhh/mid"), fx.p("hhh")] {
            let w = fx.explorer_witness(&path);
            let r = build(&fx, &w, &rs, dir_plan(&path));
            assert!(
                is_protected(&r),
                "{} order {order:?}: {}",
                path.display(),
                describe(&r)
            );
        }
    }
}

// ---------------------------------------------------------------- the home chain, real directories

fn real_chain(home: &Path) -> Vec<Identity> {
    home.ancestors().map(ident).collect()
}

#[test]
fn home_chain_should_list_home_first_then_each_ancestor_up_to_the_root_When_resolved_on_real_directories(
) {
    let fx = Fx::new();
    let rs = fx.roots();
    assert_eq!(rs.home_id(), ident(&fx.home()));
    assert_eq!(rs.home_chain().first().copied(), Some(rs.home_id()));
    assert_eq!(rs.home_chain(), real_chain(&fx.home()).as_slice());
}

#[test]
fn in_home_chain_should_be_true_for_home_and_each_ancestor_and_false_for_the_rest_When_real() {
    let fx = Fx::new();
    let rs = fx.roots();
    for p in fx.home().ancestors() {
        assert!(
            rs.in_home_chain(ident(p)),
            "{} is in the chain",
            p.display()
        );
    }
    for p in [
        fx.p("hhh/mid/sibling"),
        fx.p("hhh/mid/home/code"),
        fx.p("work"),
        fx.base.join("hhh/mid/home/code"),
    ] {
        assert!(
            !rs.in_home_chain(ident(&p)),
            "{} must not be in the chain",
            p.display()
        );
    }
    let child = fx.p("hhh/mid/home/code/proj");
    fs::create_dir_all(&child).unwrap();
    assert!(!rs.in_home_chain(ident(&child)), "a descendant of home");
}

// ---------------------------------------------------------------- a bind mount of an ancestor of $HOME

const NS_DIR: &str = "DC_BIND_DIR";

fn bind_skip(test: &str, why: &str) -> bool {
    assert!(
        std::env::var_os("DC_REQUIRE_BIND_MOUNT").is_none(),
        "{test}: bind-mount leg required but skipped ({why})"
    );
    eprintln!("skip: {test}: {why}");
    false
}

/// Re-runs `child` of this binary inside `unshare -rm` after binding `<dir>/hhh/mid` onto `<dir>/work/alias`
/// and `<dir>/hhh` onto `<dir>/work/p/node_modules`. Returns false when the leg was skipped.
fn run_in_bind_ns(child: &str, dir: &Path) -> bool {
    let out = Command::new("unshare")
        .args(["-rm", "sh", "-c"])
        .arg(
            "mount --bind \"$1/hhh/mid\" \"$1/work/alias\" && mount --bind \"$1/hhh\" \"$1/work/p/node_modules\" \
             && shift && exec \"$@\"",
        )
        .arg("sh")
        .arg(dir)
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

#[test]
fn safety_an_alternate_spelling_of_an_ancestor_of_home_should_be_refused_When_it_is_a_real_bind_mount(
) {
    let fx = Fx::new();
    fs::create_dir_all(fx.p("work/alias")).unwrap();
    fs::create_dir_all(fx.p("work/p/node_modules")).unwrap();
    fs::create_dir_all(fx.p("work/ordinary")).unwrap();
    run_in_bind_ns(
        "bind_alias_child_should_be_refused_by_the_guard_and_the_constructor_When_run_inside_the_namespace",
        &fx.base,
    );
}

#[test]
fn bind_alias_child_should_be_refused_by_the_guard_and_the_constructor_When_run_inside_the_namespace(
) {
    let Some(dir) = std::env::var_os(NS_DIR).map(PathBuf::from) else {
        return;
    };
    let decoy = tempfile::tempdir().unwrap();
    let fx = Fx {
        _td: tempfile::tempdir().unwrap(),
        decoy_home: fs::canonicalize(decoy.path()).unwrap(),
        _decoy: decoy,
        base: dir,
    };
    let alias = fx.p("work/alias");
    let nm = fx.p("work/p/node_modules");
    // The premise: a second directory entry with the identity of an ancestor of $HOME.
    assert_eq!(ident(&alias), ident(&fx.p("hhh/mid")), "alias is hhh/mid");
    assert_eq!(ident(&nm), ident(&fx.p("hhh")), "node_modules alias is hhh");
    let rs = fx.roots();
    assert!(rs.in_home_chain(ident(&alias)));
    assert!(rs.in_home_chain(ident(&nm)));
    let before = manifest(&fx.p("hhh"));

    // decide_path_common refuses the alias by identity for the explorer kind ...
    let d = decide_path_common(&facts_of(Kind::ExplorerSelection, &alias), &rs);
    assert!(
        matches!(d, GuardDecision::Skip(SkipReason::ProtectedPath)),
        "explorer alias of an ancestor of $HOME: {d:?}"
    );
    // ... and for another kind that owns the name.
    let d = decide_path_common(&facts_of(Kind::NodeModules, &nm), &rs);
    assert!(
        matches!(d, GuardDecision::Skip(SkipReason::ProtectedPath)),
        "node_modules alias of an ancestor of $HOME: {d:?}"
    );
    // The same call on an ordinary sibling directory is allowed, so the refusal is about identity.
    let ok = decide_path_common(
        &facts_of(Kind::ExplorerSelection, &fx.p("work/ordinary")),
        &rs,
    );
    assert!(matches!(ok, GuardDecision::Allow(_)), "control: {ok:?}");

    // for_allowed_roots refuses the same alias when a witness for it exists (minted by a grant that does not
    // know the chain).
    let w = fx.explorer_witness(&alias);
    let r = build(&fx, &w, &rs, dir_plan(&alias));
    assert!(is_protected(&r), "constructor: {}", describe(&r));
    let w = fx.explorer_witness(&nm);
    let r = build(&fx, &w, &rs, dir_plan(&nm));
    assert!(
        is_protected(&r),
        "constructor (grandparent): {}",
        describe(&r)
    );
    // The control is constructed.
    let w = fx.explorer_witness(&fx.p("work/ordinary"));
    assert!(build(&fx, &w, &rs, dir_plan(&fx.p("work/ordinary"))).is_ok());

    assert_eq!(manifest(&fx.p("hhh")), before, "nothing was touched");
    println!("CHILD_OK:bind_alias_child_should_be_refused_by_the_guard_and_the_constructor_When_run_inside_the_namespace");
}

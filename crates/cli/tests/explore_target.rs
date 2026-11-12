#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs,
    clippy::redundant_closure_for_method_calls,
    non_snake_case
)]
//! Task 6.6.3e: the `explorer-selection` target. It is driven directly (guard, then execute) over real
//! fixture directories, so the fd-relative checks of `execute` are reached without the apply sequence.

mod explore_rig;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use disk_cleanup_adapters::dirops::DirOps;
use disk_cleanup_adapters::targets::explorer_selection::{candidate_of, ExplorerSelectionTarget};
use disk_cleanup_adapters::testing::{At, FaultDirOps};
use disk_cleanup_adapters::RealDirOps;
use disk_cleanup_core::delete_types::{DirEntryInfo, StatInfo};
use disk_cleanup_core::explore::confirm::ReviewedPlan;
use disk_cleanup_core::ports::Fs;
use disk_cleanup_core::target::{Guarded, Target};
use disk_cleanup_core::{ApplyToken, Candidate, DeleteError, Identity, SkipReason};
use explore_rig::{manifest, Ex};

/// `DirOps` that edits what the engine sees, keyed on the name a directory was opened by.
struct Edit<D: DirOps> {
    inner: D,
    no_mnt_id: bool,
    /// An opened directory with this name reports this identity.
    lie: Option<(OsString, Identity)>,
}

struct EDir<T> {
    d: T,
    name: OsString,
}

impl<D: DirOps> DirOps for Edit<D> {
    type Dir = EDir<D::Dir>;
    type ReadDir = D::ReadDir;
    fn open_root(&self, p: &Path) -> std::io::Result<Self::Dir> {
        Ok(EDir {
            d: self.inner.open_root(p)?,
            name: OsString::new(),
        })
    }
    fn open_dir(&self, parent: &Self::Dir, name: &OsStr) -> std::io::Result<Self::Dir> {
        Ok(EDir {
            d: self.inner.open_dir(&parent.d, name)?,
            name: name.to_os_string(),
        })
    }
    fn fstat(&self, d: &Self::Dir) -> std::io::Result<StatInfo> {
        let mut st = self.inner.fstat(&d.d)?;
        if self.no_mnt_id {
            st.mnt_id = None;
        }
        if let Some((n, id)) = &self.lie {
            if *n == d.name {
                st.ident = *id;
            }
        }
        Ok(st)
    }
    fn stat_at(&self, d: &Self::Dir, name: &OsStr) -> std::io::Result<StatInfo> {
        self.inner.stat_at(&d.d, name)
    }
    fn read_dir(&self, d: &Self::Dir) -> std::io::Result<Self::ReadDir> {
        self.inner.read_dir(&d.d)
    }
    fn unlinkat(&self, d: &Self::Dir, name: &OsStr, dir: bool) -> std::io::Result<()> {
        self.inner.unlinkat(&d.d, name, dir)
    }
    fn fchmod(&self, d: &Self::Dir, mode: u32) -> std::io::Result<()> {
        self.inner.fchmod(&d.d, mode)
    }
}

fn _unused(_: DirEntryInfo) {}

fn plain(ex: &Ex) -> Edit<RealDirOps> {
    Edit {
        inner: RealDirOps::confined_to(&ex.base),
        no_mnt_id: false,
        lie: None,
    }
}

fn target<D: DirOps + 'static>(
    ex: &Ex,
    reviewed: &ReviewedPlan,
    ops: D,
) -> ExplorerSelectionTarget<D> {
    ExplorerSelectionTarget::new(
        ex.roots(),
        ex.own_state(),
        ops,
        reviewed.items().to_vec(),
        reviewed.roots().to_vec(),
    )
}

fn cand(ex: &Ex, p: &Path) -> Candidate {
    candidate_of(p, &ex.fs.lstat(p).unwrap())
}

#[test]
fn safety_target_should_remove_a_reviewed_directory_and_nothing_beside_it() {
    let ex = Ex::new();
    let a = ex.tree("app/dist", 3, 4000);
    let sibling = ex.tree("app/keep", 2, 100);
    let keep = manifest(&sibling);
    let reviewed = ex.review(&[a.clone()]);
    let t = target(&ex, &reviewed, plain(&ex));
    let c = cand(&ex, &a);
    let env = ex.env();
    assert_eq!(t.guard(&c, &env), Guarded::Allow);
    let freed = t.execute(&c, &env, &ApplyToken::for_tests()).unwrap();
    assert!(freed.entries_removed >= 8, "{freed:?}");
    assert!(!a.exists());
    assert_eq!(manifest(&sibling), keep);
}

#[test]
fn target_should_skip_a_path_the_plan_did_not_list() {
    let ex = Ex::new();
    let a = ex.tree("app/dist", 1, 100);
    let other = ex.tree("app/other", 1, 100);
    let reviewed = ex.review(&[a]);
    let t = target(&ex, &reviewed, plain(&ex));
    let env = ex.env();
    assert_eq!(
        t.guard(&cand(&ex, &other), &env),
        Guarded::Skip(SkipReason::OutsideRoot)
    );
    let before = manifest(&other);
    let err = t
        .execute(&cand(&ex, &other), &env, &ApplyToken::for_tests())
        .unwrap_err();
    assert_eq!(err.error, DeleteError::OutsideScope);
    assert_eq!(manifest(&other), before);
}

#[test]
fn safety_target_should_refuse_the_item_and_delete_nothing_When_the_root_mount_id_is_unavailable() {
    let ex = Ex::new();
    let a = ex.tree("app/dist", 2, 100);
    let before = manifest(&a);
    let reviewed = ex.review(&[a.clone()]);
    let ops = Edit {
        no_mnt_id: true,
        ..plain(&ex)
    };
    let t = target(&ex, &reviewed, ops);
    let c = cand(&ex, &a);
    let env = ex.env();
    assert_eq!(t.guard(&c, &env), Guarded::Allow);
    let err = t.execute(&c, &env, &ApplyToken::for_tests()).unwrap_err();
    assert_eq!(err.error.skip_reason(), Some(SkipReason::OutsideRoot));
    assert_eq!(manifest(&a), before);
}

#[test]
fn safety_target_should_report_changed_since_the_plan_When_the_directory_is_swapped_after_the_guard(
) {
    // Between the guard and the delete: another inode, a symlink, a plain file.
    for swap in ["inode", "symlink", "file"] {
        let ex = Ex::new();
        let a = ex.tree("app/dist", 2, 100);
        let keep = ex.tree("keepdir", 1, 50);
        let keep_before = manifest(&keep);
        let reviewed = ex.review(&[a.clone()]);
        let t = target(&ex, &reviewed, plain(&ex));
        let c = cand(&ex, &a);
        let env = ex.env();
        assert_eq!(t.guard(&c, &env), Guarded::Allow, "{swap}");
        std::fs::remove_dir_all(&a).unwrap();
        let _pin = ex.tree("pin", 1, 10);
        match swap {
            "inode" => {
                std::fs::create_dir_all(a.join("new")).unwrap();
                std::fs::write(a.join("new/f"), b"hello").unwrap();
            }
            "symlink" => std::os::unix::fs::symlink(&keep, &a).unwrap(),
            _ => std::fs::write(&a, b"i am a file now").unwrap(),
        }
        let before = manifest(&a);
        let err = t.execute(&c, &env, &ApplyToken::for_tests()).unwrap_err();
        assert_eq!(err.error, DeleteError::IdentityMismatch, "{swap}");
        assert_eq!(
            err.error.skip_reason(),
            Some(SkipReason::IdentityChanged),
            "{swap}"
        );
        assert_eq!(manifest(&a), before, "{swap}");
        assert_eq!(manifest(&keep), keep_before, "{swap}");
    }
}

#[test]
fn safety_target_should_refuse_an_opened_directory_that_is_home_or_an_ancestor_by_identity() {
    let ex = Ex::new();
    let a = ex.tree("app/dist", 2, 100);
    let before = manifest(&a);
    let reviewed = ex.review(&[a.clone()]);
    // The opened `dist` reports the identity of `$HOME`: reached under another spelling, say.
    let home_id = ex.roots().home_id();
    let ops = Edit {
        lie: Some((OsString::from("dist"), home_id)),
        ..plain(&ex)
    };
    let t = target(&ex, &reviewed, ops);
    let c = cand(&ex, &a);
    let env = ex.env();
    assert_eq!(t.guard(&c, &env), Guarded::Allow);
    let err = t.execute(&c, &env, &ApplyToken::for_tests()).unwrap_err();
    // The lie also breaks the equality with the planned identity; either refusal keeps the tree.
    assert!(matches!(
        err.error,
        DeleteError::ProtectedPath | DeleteError::IdentityMismatch
    ));
    assert_eq!(manifest(&a), before);
}

#[test]
fn safety_target_should_not_enter_a_subdirectory_the_engine_sees_on_another_mount() {
    let ex = Ex::new();
    let a = ex.tree("app/dist", 2, 100);
    std::fs::create_dir_all(a.join("bind")).unwrap();
    std::fs::write(a.join("bind/precious"), b"not yours").unwrap();
    let reviewed = ex.review(&[a.clone()]);
    let ops = FaultDirOps::wrap(RealDirOps::confined_to(&ex.base))
        .report_mnt_id(At::Name(OsString::from("bind")), 99_999)
        .build();
    let t = target(&ex, &reviewed, ops);
    let c = cand(&ex, &a);
    let env = ex.env();
    assert_eq!(t.guard(&c, &env), Guarded::Allow);
    let _ = t.execute(&c, &env, &ApplyToken::for_tests());
    assert_eq!(
        std::fs::read(a.join("bind/precious")).unwrap(),
        b"not yours"
    );
}

#[test]
fn target_should_give_the_same_result_for_nested_roots_in_either_order() {
    let mut results: Vec<(u64, Vec<PathBuf>)> = Vec::new();
    for order in [0, 1] {
        let ex = Ex::new();
        let a = ex.tree("app/sub/dist", 2, 100);
        let inner = ex.code.join("app");
        let roots_list = if order == 0 {
            vec![ex.code.clone(), inner]
        } else {
            vec![inner, ex.code.clone()]
        };
        let roots = ex.roots_of(&roots_list);
        let reviewed = ex.review_with(&[a.clone()], &roots, &ex.real_sizer());
        let t = ExplorerSelectionTarget::new(
            roots,
            ex.own_state(),
            plain(&ex),
            reviewed.items().to_vec(),
            reviewed.roots().to_vec(),
        );
        let c = cand(&ex, &a);
        let env = ex.env();
        assert_eq!(t.guard(&c, &env), Guarded::Allow, "order {order}");
        let freed = t.execute(&c, &env, &ApplyToken::for_tests()).unwrap();
        assert!(!a.exists(), "order {order}");
        results.push((freed.entries_removed, Vec::new()));
    }
    assert_eq!(results[0], results[1]);
}

#[test]
fn explorer_selection_target_should_build_its_policy_only_through_the_witness_constructor() {
    let src = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../adapters/src/targets/explorer_selection.rs"),
    )
    .unwrap();
    let code: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(code.contains("DeletePolicy::for_allowed_roots("));
    assert!(
        !code.contains("DeletePolicy {"),
        "a struct literal of DeletePolicy"
    );
    assert!(!code.contains("AllowedTarget::Exact") && !code.contains("AllowedTarget::Within"));
    assert!(
        !code.contains("for_allowed("),
        "the loose-argument constructor"
    );
}

#[test]
fn candidate_of_should_say_whether_the_entry_is_a_directory() {
    use disk_cleanup_core::KindFacts;
    let ex = Ex::new();
    let d = ex.tree("app/dist", 1, 100);
    let f = d.join("f0");
    for (p, want) in [(&d, true), (&f, false)] {
        let c = cand(&ex, p);
        let KindFacts::Explorer(facts) = c.facts() else {
            panic!("not an explorer candidate");
        };
        assert_eq!(facts.is_dir, want, "{p:?}");
    }
}

#[test]
fn target_should_name_its_selector_and_discover_the_reviewed_marks_that_still_exist() {
    use disk_cleanup_core::selector::Selection;
    use disk_cleanup_core::SelectorId;
    let ex = Ex::new();
    let a = ex.tree("app/dist", 1, 100);
    let b = ex.tree("app/out", 1, 100);
    let unreviewed = ex.tree("app/other", 1, 100);
    let reviewed = ex.review(&[a.clone(), b.clone()]);
    let t = target(&ex, &reviewed, plain(&ex));
    assert_eq!(t.name(), "explorer-selection");
    assert_eq!(t.selectors(), &[SelectorId::ExplorerSelection]);
    std::fs::remove_dir_all(&b).unwrap();
    let env = ex.env();
    let found = t.discover(&env, &Selection::explorer(vec![a.clone(), b, unreviewed]));
    assert_eq!(
        found.len(),
        1,
        "the gone mark and the unreviewed one are not items"
    );
    assert_eq!(found[0].id(), cand(&ex, &a).id());
}

#[test]
fn target_should_call_a_vanished_or_replaced_path_changed_since_the_plan_and_an_unreadable_one_protected(
) {
    use std::os::unix::fs::PermissionsExt;
    // Gone before the guard.
    let ex = Ex::new();
    let a = ex.tree("app/dist", 1, 100);
    let reviewed = ex.review(&[a.clone()]);
    let t = target(&ex, &reviewed, plain(&ex));
    let c = cand(&ex, &a);
    std::fs::remove_dir_all(&a).unwrap();
    let env = ex.env();
    assert_eq!(
        t.guard(&c, &env),
        Guarded::Skip(SkipReason::IdentityChanged)
    );

    // A parent component replaced by a plain file (ENOTDIR).
    let ex = Ex::new();
    let a = ex.tree("proj/dist", 1, 100);
    let reviewed = ex.review(&[a.clone()]);
    let t = target(&ex, &reviewed, plain(&ex));
    let c = cand(&ex, &a);
    std::fs::remove_dir_all(ex.code.join("proj")).unwrap();
    std::fs::write(ex.code.join("proj"), b"file").unwrap();
    let env = ex.env();
    assert_eq!(
        t.guard(&c, &env),
        Guarded::Skip(SkipReason::IdentityChanged)
    );

    // An unreadable parent fails closed with another reason.
    if is_root() {
        return;
    }
    let ex = Ex::new();
    let a = ex.tree("proj/dist", 1, 100);
    let reviewed = ex.review(&[a.clone()]);
    let t = target(&ex, &reviewed, plain(&ex));
    let c = cand(&ex, &a);
    let proj = ex.code.join("proj");
    std::fs::set_permissions(&proj, std::fs::Permissions::from_mode(0o000)).unwrap();
    let env = ex.env();
    let got = t.guard(&c, &env);
    std::fs::set_permissions(&proj, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(got, Guarded::Skip(SkipReason::ProtectedPath));
}

fn is_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").unwrap().uid() == 0
}

#[test]
fn safety_target_should_report_changed_since_the_plan_When_the_directory_vanishes_after_the_guard()
{
    let ex = Ex::new();
    let a = ex.tree("app/dist", 2, 100);
    let reviewed = ex.review(&[a.clone()]);
    let t = target(&ex, &reviewed, plain(&ex));
    let c = cand(&ex, &a);
    let env = ex.env();
    assert_eq!(t.guard(&c, &env), Guarded::Allow);
    std::fs::remove_dir_all(&a).unwrap();
    let err = t.execute(&c, &env, &ApplyToken::for_tests()).unwrap_err();
    assert_eq!(err.error, DeleteError::IdentityMismatch);
}

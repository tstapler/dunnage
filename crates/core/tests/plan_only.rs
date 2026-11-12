#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! `--only PATH` (Story 6.4.2, Task 6.4.2a): the value forms, the exact-match filter, the strict and tolerant
//! planners, the plan-id binding and the 24 KiB quoted cap (X-145, X-147).

mod common;

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use common::{build_dir, selection as sel_of, NOW};
use dunnage_core::plan::{non_items, plan_only, OnlyMode, Plan, PlanId};
use dunnage_core::ports::{merge_saved, SAVED_ENTRY_MAX_BYTES};
use dunnage_core::selector::{Selection, ONLY_MAX_BYTES, ONLY_MAX_VALUES, ONLY_QUOTED_MAX_BYTES};
use dunnage_core::target::Guarded;
use dunnage_core::{Candidate, SelectorId};

fn builds() -> Vec<Candidate> {
    (1..=3).map(|i| build_dir(i, 4096, 70)).collect()
}

fn item(i: u64) -> PathBuf {
    format!("/home/u/code/p{i}/node_modules").into()
}

fn with_only(only: &[PathBuf]) -> Selection {
    let mut s = sel_of(&[SelectorId::BuildDirs]);
    s.only = only.to_vec();
    s
}

fn guard(_: &Candidate) -> Guarded {
    Guarded::Allow
}

fn will_paths(p: &Plan) -> Vec<String> {
    p.items
        .iter()
        .map(|i| format!("{:?}", i.candidate.locator()))
        .collect()
}

#[test]
fn flags_should_sort_dedupe_quote_and_place_only_last() {
    let mut s = sel_of(&[SelectorId::BuildDirs]);
    s.only = vec![
        "/b/target".into(),
        "/a/node modules".into(),
        "/b/target".into(),
    ];
    s.keep_newest = Some(2);
    assert_eq!(
        s.flags(),
        "--keep-newest 2 --only '/a/node modules' --only /b/target"
    );
}

#[test]
fn only_values_should_be_absolute_canonical_and_within_the_limits() {
    let bad = [
        "",
        "relative/path",
        "/a/./b",
        "/a/../b",
        "/a//b",
        "/a/b/",
        "/",
        "/a/.",
        "/a/..",
    ];
    for v in bad {
        let err = with_only(&[v.into()]).validate_only().unwrap_err();
        assert!(
            err.0.starts_with("error: invalid value ") && err.0.ends_with("Nothing was deleted."),
            "{v:?}: {err}"
        );
    }
    for v in ["/a/b", "/a/.hidden", "/a/..b", "/a/b..", "/a/b.c"] {
        assert!(with_only(&[v.into()]).validate_only().is_ok(), "{v}");
    }

    let long = |n: usize| PathBuf::from(format!("/{}", "x".repeat(n - 1)));
    assert!(with_only(&[long(ONLY_MAX_BYTES)]).validate_only().is_ok());
    let err = with_only(&[long(ONLY_MAX_BYTES + 1)])
        .validate_only()
        .unwrap_err();
    assert!(err.0.contains("longer than 512 bytes"), "{err}");
    assert!(
        !err.0.contains("xxxxxxxx"),
        "a long value is not echoed whole"
    );

    let n = |k: usize| -> Vec<PathBuf> { (0..k).map(|i| format!("/a/n{i}").into()).collect() };
    assert!(with_only(&n(ONLY_MAX_VALUES)).validate_only().is_ok());
    let err = with_only(&n(ONLY_MAX_VALUES + 1))
        .validate_only()
        .unwrap_err();
    assert!(err.0.contains("too many values"), "{err}");
    // Duplicates are one value.
    let mut dup = n(ONLY_MAX_VALUES);
    dup.extend(n(ONLY_MAX_VALUES));
    assert!(with_only(&dup).validate_only().is_ok());
}

fn plain_512(i: usize) -> PathBuf {
    let head = format!("/p{i:02}/");
    format!("{head}{}", "x".repeat(ONLY_MAX_BYTES - head.len())).into()
}

fn control_512(i: u8) -> PathBuf {
    let mut b = vec![b'/', b'a' + i];
    b.resize(ONLY_MAX_BYTES, 0x01);
    OsString::from_vec(b).into()
}

#[test]
fn the_quoted_flags_should_stay_under_24_kib_and_round_trip_through_last_plan() {
    // 20 plain 512-byte values: about 10 KiB, accepted, and a saved entry keeps the flags whole.
    let plain: Vec<PathBuf> = (0..20).map(plain_512).collect();
    let s = with_only(&plain);
    s.validate_only().unwrap();
    let flags = s.flags();
    assert!(
        flags.len() < ONLY_QUOTED_MAX_BYTES && flags.len() > 10_000,
        "{}",
        flags.len()
    );
    assert_eq!(s.only_flags_len(), flags.len());

    // 20 control-byte values quote to about 41 KiB: refused as a value error.
    let hostile: Vec<PathBuf> = (0..20).map(control_512).collect();
    let s = with_only(&hostile);
    assert!(s.only_flags_len() > 40_000, "{}", s.only_flags_len());
    let err = s.validate_only().unwrap_err();
    assert!(err.0.contains("once quoted"), "{err}");

    // 6 of them (about 12 KiB) are accepted and the saved entry round-trips whole.
    let six = with_only(&hostile[..6]);
    six.validate_only().unwrap();
    let plan = Plan::build(&six, NOW, builds(), &guard);
    let mut saved = plan.saved_selection(&six, 1000, NOW);
    saved.items = (0..400)
        .map(|i| {
            dunnage_core::ItemKey::new(&format!("build-dirs:/home/u/code/{i}/{}", "y".repeat(150)))
        })
        .collect();
    let got = merge_saved(vec![], vec![saved], 1000, NOW);
    assert_eq!(got[0].flags, six.flags());
    assert!(got[0].items_truncated);
    assert!(serde_json::to_vec(&got[0]).unwrap().len() <= SAVED_ENTRY_MAX_BYTES);
    let back: dunnage_core::SavedSelection =
        serde_json::from_slice(&serde_json::to_vec(&got[0]).unwrap()).unwrap();
    assert_eq!(back.flags, six.flags());
}

#[test]
fn only_should_select_exactly_the_named_item_and_change_the_plan_id() {
    let all = sel_of(&[SelectorId::BuildDirs]);
    let unrestricted = Plan::build(&all, NOW, builds(), &guard);
    let s = with_only(&[item(2)]);
    let plan = Plan::build(&s, NOW, builds(), &guard);
    assert_eq!(plan.items.len(), 1, "{:?}", will_paths(&plan));
    assert!(will_paths(&plan)[0].contains("p2/node_modules"));
    assert_ne!(
        PlanId::for_selection(&plan, &s),
        PlanId::for_selection(&unrestricted, &all)
    );
    // Order and duplicates of the values do not change the id.
    let a = with_only(&[item(1), item(3)]);
    let b = with_only(&[item(3), item(1), item(3)]);
    assert_eq!(
        PlanId::for_selection(&Plan::build(&a, NOW, builds(), &guard), &a),
        PlanId::for_selection(&Plan::build(&b, NOW, builds(), &guard), &b)
    );
}

#[test]
fn the_match_should_be_byte_exact_equality_with_the_discovered_path() {
    let nested = {
        let mut c = builds();
        c.push(common::build_dir_at(
            "/h/code/app/node_modules/x/node_modules",
        ));
        c.push(common::build_dir_at("/h/code/app/node_modules"));
        c
    };
    let s = with_only(&["/h/code/app/node_modules".into()]);
    assert!(non_items(&s, &nested).is_empty());
    let plan = Plan::build(&s, NOW, nested.clone(), &guard);
    assert_eq!(plan.items.len(), 1, "the nested item is not selected");

    for spelling in [
        "/h/code/app/node_modules/",
        "/h/code//app/node_modules",
        "/h/code/./app/node_modules",
        "/h/code/x/../app/node_modules",
        "/h/code-link/app/node_modules",
        "/h/code/app",
        "/h/code/APP/node_modules",
        "/h/code/app/node_modules/x",
        "/h/code/ap\u{0070}\u{0301}/node_modules",
    ] {
        let s = with_only(&[spelling.into()]);
        assert_eq!(
            non_items(&s, &nested),
            vec![std::path::Path::new(spelling)],
            "{spelling}"
        );
    }
    // An item of an unselected kind is not an item of the selected ones.
    let mut caches = sel_of(&[SelectorId::Caches]);
    caches.only = vec!["/h/code/app/node_modules".into()];
    assert_eq!(non_items(&caches, &nested).len(), 1);
}

#[test]
fn strict_planning_should_refuse_a_non_item_with_the_exit_64_text() {
    let s = with_only(&[item(1), "/h/gone/node_modules".into()]);
    let err = plan_only(&s, NOW, builds(), &guard, OnlyMode::Strict).unwrap_err();
    assert!(
        err.starts_with("error: --only /h/gone/node_modules is not an item of the selected kinds (build-dirs). It may be new, under the age floor"),
        "{err}"
    );
    assert!(err.ends_with("Nothing was deleted."));
}

#[test]
fn tolerant_planning_should_plan_the_survivors_and_report_the_stale_marks() {
    let s = with_only(&[item(1), item(2), "/h/stale/node_modules".into()]);
    let r = plan_only(&s, NOW, builds(), &guard, OnlyMode::Tolerant).unwrap();
    assert_eq!(r.plan.items.len(), 2);
    assert_eq!(r.stale, vec![PathBuf::from("/h/stale/node_modules")]);
    assert_eq!(r.selection.only, vec![item(1), item(2)]);
    assert!(!r.selection.flags().contains("stale"));

    // No survivor: an empty plan, never "everything".
    let none = with_only(&["/h/stale/node_modules".into()]);
    let r = plan_only(&none, NOW, builds(), &guard, OnlyMode::Tolerant).unwrap();
    assert!(r.plan.items.is_empty());
    assert_eq!(r.stale.len(), 1);
    assert_eq!(
        PlanId::for_selection(&r.plan, &r.selection)
            .to_string()
            .len(),
        8
    );
}

#[test]
fn tolerant_and_strict_planning_should_give_the_same_plan_id_for_the_same_survivors_and_the_cli_should_not_reach_tolerant(
) {
    // Every subset of the three items with every subset of two stale marks added.
    let stale = [
        PathBuf::from("/h/stale1/node_modules"),
        PathBuf::from("/h/stale2/node_modules"),
    ];
    for mask in 1u32..8 {
        for smask in 0u32..4 {
            let survivors: Vec<PathBuf> = (1..=3)
                .filter(|i| mask >> (i - 1) & 1 == 1)
                .map(item)
                .collect();
            let mut marks = survivors.clone();
            marks.extend(
                stale
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| smask >> i & 1 == 1)
                    .map(|(_, p)| p.clone()),
            );
            let strict_sel = with_only(&survivors);
            let strict = plan_only(&strict_sel, NOW, builds(), &guard, OnlyMode::Strict).unwrap();
            let tol_sel = with_only(&marks);
            let tol = plan_only(&tol_sel, NOW, builds(), &guard, OnlyMode::Tolerant).unwrap();
            assert_eq!(
                PlanId::for_selection(&strict.plan, &strict.selection),
                PlanId::for_selection(&tol.plan, &tol.selection),
                "{mask} {smask}"
            );
            assert_eq!(strict.selection.flags(), tol.selection.flags());
        }
    }

    // Only the planner module and the explorer's actions may name the tolerant mode.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut found = Vec::new();
    let mut stack = vec![
        root.join("core/src"),
        root.join("cli/src"),
        root.join("tui/src"),
    ];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs")
                && std::fs::read_to_string(&p).is_ok_and(|t| t.contains("OnlyMode::Tolerant"))
            {
                found.push(
                    p.strip_prefix(&root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    found.retain(|f| f != "core/src/plan.rs" && f != "cli/src/explore/actions.rs");
    assert!(found.is_empty(), "OnlyMode::Tolerant named in {found:?}");
}

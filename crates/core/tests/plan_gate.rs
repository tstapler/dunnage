#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! `merge_saved`, the set rule of `last_plan` (Task 1.1.2a). The gate tests of Story 1.1.4 join this file.

mod common;

use disk_cleanup_core::ports::{
    merge_saved, SAVED_ENTRY_MAX_BYTES, SAVED_FILE_MAX_BYTES, SAVED_MAX_ENTRIES,
};
use disk_cleanup_core::{ItemKey, SavedPlan, SavedSelection};

const NOW: i64 = 1_000_000;
const H: i64 = 3600;

fn entry(id: &str, age_secs: i64, euid: u32, keys: usize) -> SavedSelection {
    SavedSelection {
        plan_id: id.into(),
        saved_at: NOW - age_secs,
        euid,
        kinds: "build-dirs".into(),
        flags: String::new(),
        items: (0..keys)
            .map(|i| ItemKey::new(&format!("/home/u/code/{id}/{i}/{}", "x".repeat(150))))
            .collect(),
        items_truncated: false,
    }
}

fn size(v: &[SavedSelection]) -> usize {
    serde_json::to_vec(&SavedPlan {
        entries: v.to_vec(),
    })
    .unwrap()
    .len()
}

fn ids(v: &[SavedSelection]) -> Vec<&str> {
    v.iter().map(|e| e.plan_id.as_str()).collect()
}

#[test]
fn merge_saved_should_keep_unexpired_entries_refresh_same_id_evict_oldest_at_32_and_1MiB_and_never_the_current_run(
) {
    // A 25 h old is dropped, B 23 h kept, C replaced by C' (refreshed), D added; oldest first.
    let existing = vec![
        entry("A", 25 * H, 1000, 0),
        entry("B", 23 * H, 1000, 0),
        entry("C", 2 * H, 1000, 3),
    ];
    let mut c2 = entry("C", 0, 1000, 1);
    c2.flags = "--min-age 60d".into();
    let got = merge_saved(existing, vec![c2, entry("D", 0, 1000, 0)], 1000, NOW);
    assert_eq!(ids(&got), ["B", "C", "D"]);
    let c = got.iter().find(|e| e.plan_id == "C").unwrap();
    assert_eq!(
        (c.saved_at, c.items.len(), c.flags.as_str()),
        (NOW, 1, "--min-age 60d")
    );

    // 40 distinct entries: 32 remain, the 8 oldest go, the current run's entry stays.
    let many: Vec<_> = (0..40)
        .map(|i| entry(&format!("e{i:02}"), 3600 + i64::from(i) * 60, 1000, 0))
        .collect();
    let got = merge_saved(many, vec![entry("cur", 0, 1000, 0)], 1000, NOW);
    assert_eq!(got.len(), SAVED_MAX_ENTRIES);
    assert!(ids(&got).contains(&"cur"));
    assert!(!ids(&got).contains(&"e39") && ids(&got).contains(&"e08"));

    // Another euid is kept untouched, even with the same plan id.
    let got = merge_saved(
        vec![entry("A", H, 0, 2)],
        vec![entry("A", 0, 1000, 1)],
        1000,
        NOW,
    );
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].euid, 0);
    assert_eq!(got[0].items.len(), 2);

    // A future saved_at is corrupt.
    let mut future = entry("F", 0, 1000, 0);
    future.saved_at = NOW + 1;
    assert!(merge_saved(vec![future], vec![], 1000, NOW).is_empty());

    // Boundary: exactly 24 h is kept, 24 h + 1 s is dropped.
    let got = merge_saved(
        vec![
            entry("E1", 24 * H, 1000, 0),
            entry("E2", 24 * H + 1, 1000, 0),
        ],
        vec![],
        1000,
        NOW,
    );
    assert_eq!(ids(&got), ["E1"]);
}

#[test]
fn last_plan_should_cut_entries_to_32KiB_keep_the_file_under_512KiB_and_fit_the_temp_file_in_the_reserve(
) {
    // 400 keys of about 190 bytes is over 32 KiB.
    let big = entry("big", 0, 1000, 400);
    let got = merge_saved(vec![], vec![big], 1000, NOW);
    assert!(got[0].items_truncated);
    assert!(serde_json::to_vec(&got[0]).unwrap().len() <= SAVED_ENTRY_MAX_BYTES);
    assert!(
        got[0].items.len() >= 160,
        "at least 160 keys survive: {}",
        got[0].items.len()
    );

    // 20 older full-size entries plus the run's own 4: the file fits, the current four stay.
    let older: Vec<_> = (0..20)
        .map(|i| entry(&format!("old{i:02}"), 3600 + i64::from(i), 1000, 400))
        .collect();
    let mine: Vec<_> = (0..4)
        .map(|i| entry(&format!("mine{i}"), 0, 1000, 400))
        .collect();
    let got = merge_saved(older, mine, 1000, NOW);
    assert!(size(&got) <= SAVED_FILE_MAX_BYTES, "{}", size(&got));
    for i in 0..4 {
        assert!(ids(&got).contains(&format!("mine{i}").as_str()));
    }
    let older_left = got.iter().filter(|e| e.plan_id.starts_with("old")).count();
    assert!(
        older_left <= 12,
        "older entries share 393,216 bytes: {older_left}"
    );
    // The oldest by saved_at went first: the survivors are the newest olds.
    assert!(
        ids(&got).contains(&"old00"),
        "old00 is the newest (age 3600)"
    );
}

// ---- Story 1.1.4: PlanId, destructive_gate, the refusal texts -----------------------------------------

use std::path::Path;

use common::{build_dir, cache, selection as sel_of, DAY};
use disk_cleanup_core::plan::{
    destructive_gate, ChangeCause, GateRefusal, GateRenderCtx, Plan, PlanId, PlanIdArg,
};
use disk_cleanup_core::selector::{Discard, DockerVolumes, KindSelector, Selection};
use disk_cleanup_core::target::Guarded;
use disk_cleanup_core::{Candidate, SelectorId};

const T: i64 = common::NOW;
const HOME: &str = "/home/u";

fn plan_of(sel: &Selection, cands: Vec<Candidate>) -> Plan {
    Plan::build(sel, T, cands, &|_| Guarded::Allow)
}

fn three_builds() -> Vec<Candidate> {
    vec![
        build_dir(1, 4096, 70),
        build_dir(2, 4096, 30),
        build_dir(3, 4096, 3),
    ]
}

fn pid(plan: &Plan, sel: &Selection) -> PlanId {
    PlanId::for_selection(plan, sel)
}

fn saved_entry(
    plan_id: PlanId,
    age: i64,
    kinds: &str,
    flags: &str,
    keys: &[&str],
) -> SavedSelection {
    SavedSelection {
        plan_id: plan_id.to_string(),
        saved_at: T - age,
        euid: 1000,
        kinds: kinds.into(),
        flags: flags.into(),
        items: keys.iter().map(|k| ItemKey::new(k)).collect(),
        items_truncated: false,
    }
}

fn ctx<'a>(sel: &'a Selection, saved: Option<&'a SavedPlan>) -> GateRenderCtx<'a> {
    GateRenderCtx {
        selection: sel,
        saved,
        euid: 1000,
        state_dir_low: None,
        unavailable: vec![],
        home: Some(Path::new(HOME)),
    }
}

fn gate(
    sel: &Selection,
    supplied: Option<PlanId>,
    saved: Option<&SavedPlan>,
    plan: &Plan,
    now: i64,
) -> Result<(), GateRefusal> {
    destructive_gate(sel, supplied, saved, pid(plan, sel), &plan.keys(sel), now)
}

#[test]
fn plan_id_should_ignore_size_drift_and_change_when_an_item_appears_vanishes_or_becomes_skipped_When_discovery_repeats(
) {
    let sel = sel_of(&[SelectorId::BuildDirs]);
    let base = plan_of(&sel, three_builds());
    let id = pid(&base, &sel);
    assert_eq!(id.to_string().len(), 8);

    // sizes drift: same id
    let drift = plan_of(
        &sel,
        vec![
            build_dir(1, 999_999, 70),
            build_dir(2, 1, 30),
            build_dir(3, 77, 3),
        ],
    );
    assert_eq!(pid(&drift, &sel), id);
    // an item appears (old enough to be selected)
    let mut more = three_builds();
    more.push(build_dir(4, 4096, 40));
    assert_ne!(pid(&plan_of(&sel, more), &sel), id);
    // an item vanishes
    let mut fewer = three_builds();
    fewer.remove(0);
    assert_ne!(pid(&plan_of(&sel, fewer), &sel), id);
    // an item becomes Skip for another reason
    let skipped = Plan::build(&sel, T, three_builds(), &|c| {
        if c.ident().ino == 1001 {
            Guarded::Skip(disk_cleanup_core::SkipReason::Dirty)
        } else {
            Guarded::Allow
        }
    });
    assert_ne!(pid(&skipped, &sel), id);

    // different selections over ONE discovery differ only through KindNotSelected flips
    let mixed = vec![cache(1, 10), build_dir(1, 4096, 70)];
    let both = sel_of(&[SelectorId::Caches, SelectorId::BuildDirs]);
    let plan = plan_of(&both, mixed);
    let only_builds = sel_of(&[SelectorId::BuildDirs]);
    let only_caches = sel_of(&[SelectorId::Caches]);
    assert_ne!(pid(&plan, &both), pid(&plan, &only_builds));
    assert_ne!(pid(&plan, &only_builds), pid(&plan, &only_caches));
    let docker_only = sel_of(&[SelectorId::Docker]);
    let empty = pid(&plan, &docker_only);
    assert_eq!(empty, pid(&plan_of(&docker_only, vec![]), &docker_only));
}

#[test]
fn the_id_should_bind_every_selection_flag_that_changes_the_item_set_and_ignore_the_rest() {
    let base_sel = sel_of(&[SelectorId::BuildDirs]);
    let plan = plan_of(&base_sel, three_builds());
    let bare = pid(&plan, &base_sel);

    let mut min_age = base_sel.clone();
    min_age.min_age_secs = Some(60 * DAY as u64);
    assert_ne!(pid(&plan, &min_age), bare);

    let mut root = base_sel.clone();
    root.roots = vec!["/home/u/code/p1".into()];
    assert_ne!(pid(&plan, &root), bare);

    let mut min_size = base_sel.clone();
    min_size.min_size_bytes = Some(1 << 30);
    assert_ne!(pid(&plan, &min_size), bare);

    // --except of a kind that has no member here: same id
    let except = Selection::new(
        KindSelector::of(&[SelectorId::BuildDirs, SelectorId::DockerImages]),
        KindSelector::of(&[SelectorId::DockerImages]),
    );
    assert_eq!(pid(&plan, &except), bare);
}

#[test]
fn selection_flags_should_be_canonical_sorted_quoted_and_in_the_fixed_order() {
    let mut s = Selection::new(
        KindSelector::of(&[
            SelectorId::DockerVolumes,
            SelectorId::BuildDirs,
            SelectorId::Steam,
        ]),
        KindSelector::of(&[SelectorId::Steam]),
    );
    s.stale_days(7);
    s.roots = vec!["/tmp/a b".into()];
    s.keep_newest = Some(2);
    assert_eq!(s.kinds_resolved(), "build-dirs,docker-volumes");
    assert_eq!(s.flags(), "--root '/tmp/a b' --min-age 7d --keep-newest 2");

    let mut all = Selection::new(
        KindSelector::of(&[
            SelectorId::DockerVolumes,
            SelectorId::Steam,
            SelectorId::Worktrees,
            SelectorId::BuildDirs,
        ]),
        KindSelector::default(),
    );
    all.only = vec![
        "/b/target".into(),
        "/a/node_modules".into(),
        "/b/target".into(),
    ];
    all.worktree_ignored = Some(Discard::Discard);
    all.steam_saves = Some(Discard::Discard);
    all.volumes = vec!["z".into(), "a".into()];
    all.docker_volumes = Some(DockerVolumes::Named);
    all.min_size_bytes = Some(1 << 30);
    assert_eq!(
        all.flags(),
        "--min-size 1GiB --docker-volumes named --volume a --volume z --steam-saves discard --worktree-ignored discard --only /a/node_modules --only /b/target"
    );
}

#[test]
fn destructive_gate_should_be_Ok_only_when_supplied_id_is_saved_within_24h_and_equals_the_recomputed_id_When_build_dirs_docker_volumes_or_steam_selected(
) {
    let sel = sel_of(&[SelectorId::BuildDirs]);
    let plan = plan_of(&sel, three_builds());
    let id = pid(&plan, &sel);
    let keys = plan.keys(&sel);
    let saved_at = |age| SavedPlan {
        entries: vec![saved_entry(id, age, "build-dirs", "", &[])],
    };

    // missing
    assert!(matches!(
        gate(&sel, None, None, &plan, T),
        Err(GateRefusal::MissingPlanId { ref recent }) if recent.is_empty()
    ));
    // not saved at all, and saved under another id
    assert!(matches!(
        gate(&sel, Some(id), None, &plan, T),
        Err(GateRefusal::NoRecentPlan { .. })
    ));
    let other = PlanId::from_hex("0badc0de").unwrap();
    assert!(matches!(
        gate(&sel, Some(other), Some(&saved_at(60)), &plan, T),
        Err(GateRefusal::NoRecentPlan { .. })
    ));
    // 86,400 s accepted (boundary included), 86,401 s refused
    assert_eq!(
        gate(&sel, Some(id), Some(&saved_at(86_400)), &plan, T),
        Ok(())
    );
    assert!(matches!(
        gate(&sel, Some(id), Some(&saved_at(86_401)), &plan, T),
        Err(GateRefusal::Stale {
            age_secs: 86_401,
            ..
        })
    ));
    // changed: the saved id is not the recomputed one
    let wrong = SavedPlan {
        entries: vec![saved_entry(other, 60, "build-dirs", "", &[])],
    };
    assert!(matches!(
        destructive_gate(&sel, Some(other), Some(&wrong), id, &keys, T),
        Err(GateRefusal::Changed { .. })
    ));

    // the three texts of design/ux.md 1.1
    let c = ctx(&sel, None);
    let missing = GateRefusal::MissingPlanId { recent: vec![] }.render(&c);
    assert_eq!(
        missing,
        "error: destructive kinds (build-dirs) need --plan-id from a dry run made in the last 24h. Run 'disk-cleanup --kinds build-dirs' first and use the plan-id on its Next: line. Recent dry runs: none. Nothing was deleted."
    );
    let some = SavedPlan {
        entries: vec![saved_entry(other, 3 * 3600, "build-dirs", "", &[])],
    };
    let err = gate(&sel, None, Some(&some), &plan, T).unwrap_err();
    assert_eq!(
        err.render(&ctx(&sel, Some(&some))),
        "error: destructive kinds (build-dirs) need --plan-id from a dry run made in the last 24h. Run 'disk-cleanup --kinds build-dirs' first and use the plan-id on its Next: line. Recent dry runs: 0badc0de ('--kinds build-dirs', 3h ago). Nothing was deleted."
    );
    let err = gate(&sel, Some(other), Some(&some), &plan, T);
    assert_eq!(
        err,
        Err(GateRefusal::Changed {
            supplied: other,
            recomputed: id,
            cause: ChangeCause::ItemsMoved {
                vanished: vec![],
                appeared: vec![],
                incomplete: false
            },
        })
    );
    let unknown = gate(&sel, Some(id), Some(&some), &plan, T).unwrap_err();
    assert_eq!(
        unknown.render(&ctx(&sel, Some(&some))),
        format!("error: no dry run with plan-id {id} in the last 24h (recent: 0badc0de for '--kinds build-dirs', 3h ago). Dry run again, then use the plan-id on its Next: line: disk-cleanup --kinds build-dirs. Nothing was deleted.")
    );
    let none = gate(&sel, Some(id), None, &plan, T).unwrap_err();
    assert!(none.render(&ctx(&sel, None)).contains("(recent: none)"));
    let stale = SavedPlan {
        entries: vec![saved_entry(id, 90_000, "build-dirs", "", &[])],
    };
    let stale_err = gate(&sel, Some(id), Some(&stale), &plan, T).unwrap_err();
    assert!(stale_err
        .render(&ctx(&sel, Some(&stale)))
        .starts_with(&format!(
            "error: no dry run with plan-id {id} in the last 24h (recent: none)."
        )));
}

#[test]
fn gate_should_not_apply_to_caches_and_worktrees_but_should_verify_a_supplied_id_for_any_selection_When_checked(
) {
    let safe = sel_of(&[SelectorId::Caches, SelectorId::Worktrees]);
    let plan = plan_of(&safe, vec![cache(1, 10)]);
    assert_eq!(gate(&safe, None, None, &plan, T), Ok(()));

    let mut discard = safe.clone();
    discard.worktree_ignored = Some(Discard::Discard);
    assert!(matches!(
        gate(&discard, None, None, &plan, T),
        Err(GateRefusal::MissingPlanId { .. })
    ));
    let text = gate(&discard, None, None, &plan, T)
        .unwrap_err()
        .render(&ctx(&discard, None));
    assert!(
        text.starts_with("error: destructive kinds (--worktree-ignored discard)"),
        "{text}"
    );

    // a wrong supplied id is refused even where none is required
    let wrong = PlanId::from_hex("deadbeef").unwrap();
    assert!(matches!(
        gate(&safe, Some(wrong), None, &plan, T),
        Err(GateRefusal::NoRecentPlan { .. })
    ));
    // docker and steam: the same rule
    for ids in [&[SelectorId::DockerVolumes][..], &[SelectorId::Steam][..]] {
        let s = sel_of(ids);
        let p = plan_of(&s, vec![]);
        assert!(matches!(
            gate(&s, None, None, &p, T),
            Err(GateRefusal::MissingPlanId { .. })
        ));
    }
    // docker (guarded) and caches need none
    let docker = sel_of(&[SelectorId::Docker]);
    assert_eq!(
        gate(&docker, None, None, &plan_of(&docker, vec![]), T),
        Ok(())
    );
}

#[test]
fn changed_should_name_vanished_and_appeared_items_and_flag_an_incomplete_list() {
    let sel = sel_of(&[SelectorId::BuildDirs]);
    let id = PlanId::from_hex("91be04d7").unwrap();
    let re = PlanId::from_hex("c4410aa2").unwrap();
    let key = |n: &str| format!("node-modules:/home/u/code/{n}/node_modules");
    let ks: Vec<String> = ["a", "b", "c", "d"].iter().map(|n| key(n)).collect();
    let kr: Vec<&str> = ks.iter().map(String::as_str).collect();
    let saved = SavedPlan {
        entries: vec![saved_entry(id, 600, "build-dirs", "", &kr)],
    };
    let now_keys: Vec<ItemKey> = ["c", "d", "e"]
        .iter()
        .map(|n| ItemKey::new(&key(n)))
        .collect();
    let err = destructive_gate(&sel, Some(id), Some(&saved), re, &now_keys, T).unwrap_err();
    match &err {
        GateRefusal::Changed {
            cause:
                ChangeCause::ItemsMoved {
                    vanished,
                    appeared,
                    incomplete,
                },
            ..
        } => {
            assert_eq!((vanished.len(), appeared.len(), *incomplete), (2, 1, false));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        err.render(&ctx(&sel, Some(&saved))),
        "error: plan-id 91be04d7 does not match the plan just computed (c4410aa2): 2 items vanished (/home/u/code/a/node_modules, /home/u/code/b/node_modules), 1 appeared (/home/u/code/e/node_modules). Dry run again, then use the new plan-id on its Next: line: disk-cleanup --kinds build-dirs. If an earlier apply was interrupted or partly failed, this is expected. Nothing was deleted."
            .replace("/home/u/", "~/")
    );

    // more than three on a side: first three, then +N more
    let many: Vec<String> = (0..6).map(|i| key(&format!("m{i}"))).collect();
    let mr: Vec<&str> = many.iter().map(String::as_str).collect();
    let saved6 = SavedPlan {
        entries: vec![saved_entry(id, 600, "build-dirs", "", &mr)],
    };
    let err = destructive_gate(&sel, Some(id), Some(&saved6), re, &[], T).unwrap_err();
    let text = err.render(&ctx(&sel, Some(&saved6)));
    assert!(text.contains("6 items vanished (~/code/m0/node_modules, ~/code/m1/node_modules, ~/code/m2/node_modules, +3 more)"), "{text}");

    // a saved list cut at the plan's limit says so
    let mut cut = saved6.clone();
    cut.entries[0].items_truncated = true;
    let text = destructive_gate(&sel, Some(id), Some(&cut), re, &[], T)
        .unwrap_err()
        .render(&ctx(&sel, Some(&cut)));
    assert!(
        text.contains("(list incomplete: the saved plan keeps the first 6 items)"),
        "{text}"
    );

    // a saved selection with no keys: no lists, the rest of the text unchanged
    let bare = SavedPlan {
        entries: vec![saved_entry(id, 600, "build-dirs", "", &[])],
    };
    let text = destructive_gate(&sel, Some(id), Some(&bare), re, &now_keys[..0], T)
        .unwrap_err()
        .render(&ctx(&sel, Some(&bare)));
    assert!(text.starts_with("error: plan-id 91be04d7 does not match the plan just computed (c4410aa2). Dry run again"), "{text}");
}

#[test]
fn changed_should_say_the_selection_differs_and_end_with_the_paste_ready_dry_run() {
    let id = PlanId::from_hex("91be04d7").unwrap();
    let re = PlanId::from_hex("c4410aa2").unwrap();
    let sel = sel_of(&[SelectorId::BuildDirs, SelectorId::DockerVolumes]);
    let saved = SavedPlan {
        entries: vec![saved_entry(
            id,
            600,
            "build-dirs",
            "--min-age 60d",
            &["node-modules:/x"],
        )],
    };
    let err = destructive_gate(
        &sel,
        Some(id),
        Some(&saved),
        re,
        &[ItemKey::new("docker-volume:v")],
        T,
    )
    .unwrap_err();
    assert!(matches!(
        err,
        GateRefusal::Changed {
            cause: ChangeCause::SelectionDiffers { .. },
            ..
        }
    ));
    let text = err.render(&ctx(&sel, Some(&saved)));
    assert_eq!(
        text,
        "error: plan-id 91be04d7 was a dry run of '--kinds build-dirs --min-age 60d'; this command selects '--kinds build-dirs,docker-volumes', so the ids differ. Use the Next: line of that dry run unchanged, or dry run this selection: disk-cleanup --kinds build-dirs,docker-volumes. Nothing was deleted."
    );
    assert!(!text.contains("vanished") && !text.contains("appeared"));

    // the same selection typed in another order is the same selection: ItemsMoved
    let swapped = Selection::new(
        KindSelector::parse("docker-volumes,build-dirs", &SelectorId::ALL).unwrap(),
        KindSelector::default(),
    );
    let saved_same = SavedPlan {
        entries: vec![saved_entry(
            id,
            600,
            "build-dirs,docker-volumes",
            "",
            &["node-modules:/x"],
        )],
    };
    let err = destructive_gate(&swapped, Some(id), Some(&saved_same), re, &[], T).unwrap_err();
    assert!(matches!(
        err,
        GateRefusal::Changed {
            cause: ChangeCause::ItemsMoved { .. },
            ..
        }
    ));
}

#[test]
fn changed_should_name_vanished_and_appeared_items_and_docker_down_and_uid_cases_should_read_as_specified(
) {
    let sel = sel_of(&[SelectorId::BuildDirs]);
    let id = PlanId::from_hex("91be04d7").unwrap();
    let re = PlanId::from_hex("c4410aa2").unwrap();
    let saved = SavedPlan {
        entries: vec![saved_entry(id, 600, "build-dirs", "", &[])],
    };
    let err = destructive_gate(&sel, Some(id), Some(&saved), re, &[], T).unwrap_err();

    // euid 1000 saved, euid 0 now
    let mut c = ctx(&sel, Some(&saved));
    c.euid = 0;
    let text = err.render(&c);
    assert!(text.contains("That dry run was made as uid 1000; this run is root, which sees a different HOME and skips your user caches, so its ids differ: dry run and apply as the same user. Nothing was deleted."), "{text}");

    // under 1 MiB free in the state directory
    let mut c = ctx(&sel, Some(&saved));
    c.state_dir_low = Some(("~/.local/state/disk-cleanup".into(), 0));
    let text = err.render(&c);
    assert!(text.contains("The state directory ~/.local/state/disk-cleanup has 0 B free: free about 1 MiB there, then dry run again. Nothing was deleted."), "{text}");

    // the missing and the unknown refusals carry the state-directory sentence too
    let missing = GateRefusal::MissingPlanId { recent: vec![] }.render(&c);
    assert!(missing.contains("The state directory"), "{missing}");

    // docker was unreachable now
    let both = sel_of(&[SelectorId::BuildDirs, SelectorId::DockerVolumes]);
    let mut c = ctx(&both, Some(&saved));
    c.unavailable = vec![SelectorId::DockerVolumes];
    let err = GateRefusal::Changed {
        supplied: id,
        recomputed: re,
        cause: ChangeCause::ItemsMoved {
            vanished: vec![],
            appeared: vec![],
            incomplete: false,
        },
    };
    let text = err.render(&c);
    assert!(text.contains("docker-volumes was not reachable now, which changes the id (it was in your dry run). Start Docker and re-run the same command, or dry run again without docker-volumes."), "{text}");
}

#[test]
fn plan_id_arg_should_reject_seven_nine_nonhex_0x_and_empty_and_lowercase_uppercase() {
    for bad in ["91be04", "91be04d7a", "zzzzzzzz", "0x7f3a91c", ""] {
        let e = bad.parse::<PlanIdArg>().unwrap_err();
        assert_eq!(
            e.to_string(),
            format!("error: invalid value '{bad}' for '--plan-id <ID>': a plan-id is 8 hex digits (for example 7f3a91c2), copied from the Next: line of a dry run.")
        );
    }
    let upper = "91BE04D7".parse::<PlanIdArg>().unwrap();
    let lower = "91be04d7".parse::<PlanIdArg>().unwrap();
    assert_eq!(upper.plan_id(), lower.plan_id());
    assert_eq!(upper.plan_id().to_string(), "91be04d7");
}

#[test]
fn recent_dry_runs_should_list_at_most_the_three_newest_unexpired_entries() {
    let sel = sel_of(&[SelectorId::BuildDirs]);
    let plan = plan_of(&sel, three_builds());
    let mk = |h: &str, age| saved_entry(PlanId::from_hex(h).unwrap(), age, "build-dirs", "", &[]);
    let saved = SavedPlan {
        entries: vec![
            mk("00000001", 5 * 3600),
            mk("00000002", 3600),
            mk("00000003", 2 * 3600),
            mk("00000004", 4 * 3600),
            mk("00000005", 30 * 3600),
        ],
    };
    let text = gate(&sel, None, Some(&saved), &plan, T)
        .unwrap_err()
        .render(&ctx(&sel, Some(&saved)));
    assert!(text.contains("Recent dry runs: 00000002 ('--kinds build-dirs', 1h ago), 00000003 ('--kinds build-dirs', 2h ago), 00000004 ('--kinds build-dirs', 4h ago)."), "{text}");
}

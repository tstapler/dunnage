#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 1.2.3: placeholders are filled, never printed (T-532).

mod support;

use std::path::PathBuf;

use disk_cleanup_cli::reasontext::{next_action, reason, ReasonFacts, TextCtx};
use disk_cleanup_core::kind::SelectorId;
use disk_cleanup_core::outcome::SkipReason;
use support::*;

fn has_placeholder(s: &str) -> bool {
    let b: Vec<char> = s.chars().collect();
    let angle = b.iter().enumerate().any(|(i, c)| {
        *c == '<'
            && b[i + 1..].iter().take_while(|x| **x != '>').count() < b.len() - i - 1
            && b[i + 1..]
                .iter()
                .take_while(|x| **x != '>')
                .all(|x| x.is_alphanumeric() || *x == ' ')
            && b.get(i + 1).is_some_and(|n| n.is_alphabetic())
    });
    let bare_n = s.split(|c: char| !c.is_alphanumeric()).any(|w| w == "N");
    angle || bare_n
}

fn full_facts() -> ReasonFacts {
    ReasonFacts {
        unpushed: Some(3),
        referrer: Some("pg-compose".into()),
        process: Some(("nvim".into(), 4242)),
        ignored: Some((12, 3 << 20)),
        discards: None,
        duplicates: Some(3),
        name: Some("my volume".into()),
        path: Some(PathBuf::from("/h/x /etc")),
        detail: Some("a detail".into()),
    }
}

#[test]
fn no_placeholder_should_survive_rendering_When_every_skip_reason_is_rendered_with_and_without_its_facts(
) {
    let sel = default_selection();
    let exe = PathBuf::from(EXE);
    for r in SkipReason::ALL {
        for facts in [ReasonFacts::default(), full_facts()] {
            for ascii_only in [true, false] {
                let ctx = TextCtx {
                    selection: &sel,
                    selector: SelectorId::Worktrees,
                    exe: &exe,
                    ascii_only,
                };
                let t = reason(r, &facts, &ctx);
                let a = next_action(r, &facts, &ctx);
                assert!(!has_placeholder(&t), "{r:?} reason: {t}");
                assert!(!has_placeholder(&a), "{r:?} action: {a}");
                assert!(!t.is_empty() && !a.is_empty());
            }
        }
    }
}

#[test]
fn facts_should_fill_the_clauses_of_the_reason_texts() {
    let sel = default_selection();
    let exe = PathBuf::from(EXE);
    let ctx = TextCtx {
        selection: &sel,
        selector: SelectorId::Worktrees,
        exe: &exe,
        ascii_only: true,
    };
    let f = full_facts();
    assert_eq!(
        reason(SkipReason::UnpushedCommits, &f, &ctx),
        "3 unpushed commits"
    );
    assert_eq!(
        reason(SkipReason::InUse, &f, &ctx),
        "in use: nvim (pid 4242) has its working directory or an open file under this worktree"
    );
    assert_eq!(reason(SkipReason::HasIgnoredFiles, &f, &ctx), "holds ignored files that git would delete with it (12 files, 3.0 MiB, for example .env, *.sqlite)");
    assert_eq!(
        reason(SkipReason::VolumeReferenced, &f, &ctx),
        "volume used by container/compose project pg-compose"
    );
    assert_eq!(
        reason(SkipReason::TooRecent, &ReasonFacts::default(), &ctx)
            .split(' ')
            .nth(3),
        Some("14d")
    );
    assert_eq!(
        reason(SkipReason::InUse, &ReasonFacts::default(), &ctx),
        "in use by a live process"
    );
    assert_eq!(
        next_action(SkipReason::Locked, &f, &ctx),
        "git worktree unlock -- '/h/x /etc' if intended"
    );
    assert_eq!(
        next_action(SkipReason::NeedsRoot, &f, &ctx),
        "sudo /home/tstapler/.cargo/bin/disk-cleanup --apply --kinds worktrees"
    );
    let mut sel2 = default_selection();
    sel2.min_age_secs = Some(3600);
    let ctx2 = TextCtx {
        selection: &sel2,
        selector: SelectorId::Worktrees,
        exe: &exe,
        ascii_only: true,
    };
    assert!(
        reason(SkipReason::TooRecent, &ReasonFacts::default(), &ctx2).contains("--min-age 1d"),
        "the 24h worktree floor applies"
    );
}

#[test]
fn cache_apt_needs_root_should_name_the_finest_id_with_a_quoted_binary_path_and_never_caches() {
    let sel = default_selection();
    let exe = PathBuf::from("/opt/my tools/disk-cleanup");
    let ctx = TextCtx {
        selection: &sel,
        selector: SelectorId::CacheApt,
        exe: &exe,
        ascii_only: true,
    };
    let text = next_action(SkipReason::NeedsRoot, &ReasonFacts::default(), &ctx);
    assert_eq!(
        text,
        "sudo '/opt/my tools/disk-cleanup' --apply --kinds cache-apt"
    );
    assert!(!text.contains("--kinds caches"));
}

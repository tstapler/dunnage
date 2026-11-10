#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! `merge_saved`, the set rule of `last_plan` (Task 1.1.2a). The gate tests of Story 1.1.4 join this file.

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

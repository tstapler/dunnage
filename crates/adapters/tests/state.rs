#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! State dir, reserves, `RealPlanStore` and `outcome.jsonl` (Story 2.2.1, Tasks 2.2.1b, c, e; validation
//! T-377, T-435, T-452, T-457, T-463, T-486, T-498, T-499, UX-49, UX-52).

use std::fs::{File, OpenOptions};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;

use disk_cleanup_adapters::state::{
    append_outcome, OutcomeLine, OutcomeMode, RealPlanStore, StateDir, LOCK_BUSY_ERRNO,
    OUTCOME_ROTATE_BYTES, RESERVE_BYTES,
};
use disk_cleanup_core::ports::PlanStore;
use disk_cleanup_core::{DiskVerdict, ItemKey, SaveOutcome, SavedSelection};
use rustix::fs::{flock, FlockOperation};

#[path = "support/fullfs.rs"]
mod fullfs;

const NOW: i64 = 1_800_000_000;

fn plenty(_: &Path) -> std::io::Result<u64> {
    Ok(1 << 30)
}

fn store(dir: &Path, now: i64) -> RealPlanStore {
    let (sd, _) = StateDir::open(dir, 1000, &plenty).unwrap();
    RealPlanStore::new(sd, Arc::new(move || now))
}

fn entry(id: &str, items: usize) -> SavedSelection {
    SavedSelection {
        plan_id: id.into(),
        saved_at: 0,
        euid: 1000,
        kinds: "build-dirs".into(),
        flags: String::new(),
        items: (0..items)
            .map(|i| ItemKey::new(&format!("node-modules:/home/u/code/p{i}/node_modules")))
            .collect(),
        items_truncated: false,
    }
}

fn ids(s: &RealPlanStore) -> Vec<String> {
    s.load_last()
        .map(|p| p.entries.into_iter().map(|e| e.plan_id).collect())
        .unwrap_or_default()
}

#[test]
fn reserve_should_be_created_at_the_first_start_of_any_mode_and_not_recreated_when_the_disk_is_full(
) {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("disk-cleanup");
    let (sd, init) = StateDir::open(&dir, 1000, &plenty).unwrap();
    assert!(init.reserves_created);
    assert!(sd.reserves_note(&init).is_some());
    for r in [sd.paths.audit_reserve(), sd.paths.last_plan_reserve()] {
        let m = std::fs::metadata(&r).unwrap();
        assert_eq!(m.len(), RESERVE_BYTES);
        assert_eq!(m.mode() & 0o777, 0o600);
        assert!(std::fs::read(&r).unwrap().iter().all(|b| *b == 0));
    }
    // A second start changes nothing and prints no note.
    let (_, again) = StateDir::open(&dir, 1000, &plenty).unwrap();
    assert!(!again.reserves_created);
}

#[test]
fn reserves_should_exist_before_the_first_state_write_and_not_be_recreated_below_16MiB_available() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("s");
    let (sd, _) = StateDir::open(&dir, 1000, &plenty).unwrap();
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["audit.reserve", "last_plan.reserve"]);

    // A reserve a full-disk run truncated is re-created only with 16 MiB available.
    std::fs::write(sd.paths.audit_reserve(), "").unwrap();
    let (sd, i) = StateDir::open(&dir, 1000, &|_| Ok((16 << 20) - 1)).unwrap();
    assert!(!i.reserves_created);
    assert_eq!(
        std::fs::metadata(sd.paths.audit_reserve()).unwrap().len(),
        0
    );
    std::fs::remove_file(sd.paths.last_plan_reserve()).unwrap();
    let (_, i) = StateDir::open(&dir, 1000, &|_| Ok((16 << 20) - 1)).unwrap();
    assert!(!i.reserves_created && !sd.paths.last_plan_reserve().exists());
    // (an existing, truncated file is left alone: `exists()` is the test, the release is the full-disk run's)
    let (_, i) = StateDir::open(&dir, 1000, &|_| Ok(16 << 20)).unwrap();
    assert!(i.reserves_created && sd.paths.last_plan_reserve().exists());

    // An empty dir on a full disk: nothing is created and nothing panics.
    let full = d.path().join("full");
    let (sd, i) = StateDir::open(&full, 1000, &|_| Ok(0)).unwrap();
    assert!(!i.reserves_created && !sd.paths.audit_reserve().exists());
    let (_, i) = StateDir::open(&full, 1000, &|_| Err(std::io::Error::other("statvfs"))).unwrap();
    assert!(!i.reserves_created);
}

#[test]
fn last_plan_should_be_atomic_time_stamped_and_load_as_none_When_schema_unknown_file_truncated_or_timestamp_in_the_future(
) {
    let d = tempfile::tempdir().unwrap();
    let s = store(d.path(), NOW);
    assert!(s.load_last().is_none());
    assert_eq!(
        s.save_merged(vec![entry("aaaa0001", 2)], 1000, NOW),
        SaveOutcome::Saved
    );
    assert!(!d.path().join("last_plan.tmp").exists());
    let p = s.load_last().unwrap();
    assert_eq!(p.entries.len(), 1);
    assert_eq!((p.entries[0].saved_at, p.entries[0].euid), (NOW, 1000));
    let m = std::fs::metadata(d.path().join("last_plan")).unwrap();
    assert_eq!(m.mode() & 0o777, 0o600);

    let path = d.path().join("last_plan");
    let good = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, good.replacen("\"schema\":1", "\"schema\":9", 1)).unwrap();
    assert!(s.load_last().is_none());
    assert!(s.unreadable_warning().text().contains("last_plan"));
    std::fs::write(&path, &good[..good.len() / 2]).unwrap();
    assert!(s.load_last().is_none());
    std::fs::write(&path, &good).unwrap();
    assert!(s.load_last().is_some());
    // Dated in the future relative to this reader's clock.
    let earlier = store(d.path(), NOW - 10);
    assert!(earlier.load_last().is_none());
}

#[test]
fn two_concurrent_saves_should_keep_both_ids_under_the_advisory_lock() {
    for round in 0..20 {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (store(d.path(), NOW), store(d.path(), NOW));
        let (ta, tb) = std::thread::scope(|sc| {
            let ta = sc.spawn(|| a.save_merged(vec![entry("aaaa0001", 1)], 1000, NOW));
            let tb = sc.spawn(|| b.save_merged(vec![entry("bbbb0002", 1)], 1000, NOW));
            (ta.join().unwrap(), tb.join().unwrap())
        });
        assert_eq!(
            (ta, tb),
            (SaveOutcome::Saved, SaveOutcome::Saved),
            "round {round}"
        );
        let mut got = ids(&a);
        got.sort();
        assert_eq!(got, ["aaaa0001", "bbbb0002"], "round {round}");
    }
}

#[test]
fn plan_save_should_wait_2s_for_the_lock_then_return_NotSaved_LockBusy_with_the_warning_and_exit_0()
{
    let d = tempfile::tempdir().unwrap();
    let s = store(d.path(), NOW);
    let lock = File::create(d.path().join("last_plan.lock")).unwrap();
    flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
    let t = std::time::Instant::now();
    let out = s.save_merged(vec![entry("aaaa0001", 1)], 1000, NOW);
    let waited = t.elapsed();
    assert_eq!(out, SaveOutcome::NotSaved(LOCK_BUSY_ERRNO));
    assert!(
        waited >= std::time::Duration::from_millis(1900),
        "{waited:?}"
    );
    assert!(waited < std::time::Duration::from_secs(6), "{waited:?}");
    assert!(!d.path().join("last_plan").exists());
    assert!(disk_cleanup_core::Warning::PlanLockBusy
        .text()
        .contains("last_plan.lock busy for 2 s"));
    drop(lock);
    assert_eq!(
        s.save_merged(vec![entry("aaaa0001", 1)], 1000, NOW),
        SaveOutcome::Saved
    );
}

#[test]
fn later_dry_runs_should_not_clobber_the_copied_plan_id_and_entries_should_expire_individually_within_32_and_1MiB(
) {
    let d = tempfile::tempdir().unwrap();
    let at = |t: i64| store(d.path(), t);
    // A, then an agent's --json run and a docker run.
    at(NOW).save_merged(vec![entry("A0000001", 3)], 1000, NOW);
    at(NOW + 5).save_merged(vec![entry("B0000002", 0)], 1000, NOW + 5);
    at(NOW + 9).save_merged(vec![entry("C0000003", 0)], 1000, NOW + 9);
    assert_eq!(ids(&at(NOW + 9)), ["A0000001", "B0000002", "C0000003"]);

    // Each entry expires on its own clock.
    let h = 3600;
    let d2 = tempfile::tempdir().unwrap();
    let s2 = |t: i64| store(d2.path(), t);
    s2(NOW).save_merged(vec![entry("OLD00001", 0)], 1000, NOW);
    s2(NOW + 2 * h).save_merged(vec![entry("NEW00002", 0)], 1000, NOW + 2 * h);
    let later = NOW + 25 * h;
    assert_eq!(
        s2(later).save_merged(vec![entry("CUR00003", 0)], 1000, later),
        SaveOutcome::Saved
    );
    assert_eq!(ids(&s2(later)), ["NEW00002", "CUR00003"]);

    // 40 distinct selections: at most 32 entries and 512 KiB, the current one is never evicted.
    let d3 = tempfile::tempdir().unwrap();
    for i in 0..40 {
        let t = NOW + i;
        store(d3.path(), t).save_merged(vec![entry(&format!("{i:08x}"), 150)], 1000, t);
    }
    let s3 = store(d3.path(), NOW + 40);
    let got = ids(&s3);
    assert!(got.len() <= 32, "{}", got.len());
    assert!(got.contains(&format!("{:08x}", 39)));
    assert!(
        std::fs::metadata(d3.path().join("last_plan"))
            .unwrap()
            .len()
            <= 524_288
    );
}

#[test]
fn last_plan_should_cut_entries_to_32KiB_keep_the_file_under_512KiB_and_fit_the_temp_file_in_the_reserve(
) {
    let d = tempfile::tempdir().unwrap();
    let s = store(d.path(), NOW);
    let four: Vec<_> = (0..4)
        .map(|i| entry(&format!("D000000{i}"), 6000))
        .collect();
    assert_eq!(s.save_merged(four, 1000, NOW), SaveOutcome::Saved);
    let p = s.load_last().unwrap();
    assert_eq!(p.entries.len(), 4);
    for e in &p.entries {
        assert!(e.items_truncated);
        assert!(serde_json::to_vec(e).unwrap().len() <= 32_768);
    }
    let size = std::fs::metadata(d.path().join("last_plan")).unwrap().len();
    assert!(size <= 524_288, "{size}");
    assert!(
        size <= 4 * 32_768 + 64,
        "four entries stay near 128 KiB: {size}"
    );
    // Half the reserve, so the temp file plus the old file fit once the reserve is released.
    assert!(524_288 * 2 <= RESERVE_BYTES);
}

#[test]
fn last_plan_should_survive_a_full_tmpfs_When_the_reserve_exists_and_report_NotSaved_When_it_does_not(
) {
    if let Some(dir) = fullfs::child_dir() {
        return full_tmpfs_child(&dir);
    }
    fullfs::run_child(
        "last_plan_should_survive_a_full_tmpfs_When_the_reserve_exists_and_report_NotSaved_When_it_does_not",
        3072,
    );
}

fn full_tmpfs_child(dir: &Path) {
    const T: &str = "last_plan_should_survive_a_full_tmpfs_When_the_reserve_exists_and_report_NotSaved_When_it_does_not";
    // With reserves: the first write hits ENOSPC, the reserve is released and the retry saves.
    let with = dir.join("with");
    std::fs::create_dir(&with).unwrap();
    let s = store(&with, NOW);
    fullfs::fill(dir);
    assert!(
        std::fs::write(with.join("probe"), b"x").is_err(),
        "tmpfs is not full"
    );
    assert_eq!(
        s.save_merged(vec![entry("aaaa0001", 100)], 1000, NOW),
        SaveOutcome::SavedAfterReserve
    );
    assert_eq!(ids(&s), ["aaaa0001"]);
    assert!(!with.join("last_plan.tmp").exists());
    assert_eq!(
        std::fs::metadata(with.join("last_plan.reserve"))
            .unwrap()
            .len(),
        0
    );

    // Reserve gone (the first run on a full disk): NotSaved(ENOSPC), and the old file is untouched.
    let before = std::fs::read(with.join("last_plan")).unwrap();
    fullfs::fill(dir);
    let out = s.save_merged(vec![entry("bbbb0002", 5000)], 1000, NOW + 1);
    assert!(matches!(out, SaveOutcome::NotSaved(28 | 122)), "{out:?}");
    assert_eq!(std::fs::read(with.join("last_plan")).unwrap(), before);
    assert!(!with.join("last_plan.tmp").exists());
    fullfs::ok(T);
}

#[test]
fn outcome_jsonl_should_get_one_line_per_run_of_every_mode_rotate_at_256KiB_and_be_read_for_days_in_warn(
) {
    let d = tempfile::tempdir().unwrap();
    let (sd, _) = StateDir::open(d.path(), 1000, &plenty).unwrap();
    for (i, (mode, verdict)) in [
        (OutcomeMode::Plan, DiskVerdict::Warn),
        (OutcomeMode::Apply, DiskVerdict::Warn),
        (OutcomeMode::Status, DiskVerdict::Ok),
    ]
    .into_iter()
    .enumerate()
    {
        let line = OutcomeLine {
            schema: 1,
            ts: NOW + i as i64,
            run_id: format!("20261110T0000{i}-1234-abcd"),
            mode,
            verdict,
            free_bytes: 12_345_678_901,
            unallocated_bytes: (i != 2).then_some(1_000_000_000),
            via: (i == 1).then(|| "dc-wrapper".into()),
        };
        append_outcome(&sd.paths, None, &line).unwrap();
    }
    let text = std::fs::read_to_string(sd.paths.outcome()).unwrap();
    let v: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(v.len(), 3);
    for (l, raw) in v.iter().zip(text.lines()) {
        assert!(raw.len() <= 200, "{} bytes: {raw}", raw.len());
        for k in [
            "schema",
            "ts",
            "run_id",
            "mode",
            "verdict",
            "free_bytes",
            "unallocated_bytes",
            "via",
        ] {
            assert!(l.get(k).is_some(), "missing {k}: {raw}");
        }
    }
    assert_eq!(
        (
            v[0]["mode"].as_str(),
            v[1]["mode"].as_str(),
            v[2]["mode"].as_str()
        ),
        (Some("plan"), Some("apply"), Some("status"))
    );
    assert!(
        v[2]["unallocated_bytes"].is_null() && v[0]["via"].is_null() && v[1]["via"] == "dc-wrapper"
    );
    assert_eq!(
        std::fs::metadata(sd.paths.outcome()).unwrap().mode() & 0o777,
        0o600
    );

    // Rotation: past 256 KiB the file becomes outcome.jsonl.1 (replacing an older one) and a new one starts.
    let pad = vec![b'x'; OUTCOME_ROTATE_BYTES as usize];
    std::fs::write(sd.paths.outcome(), &pad).unwrap();
    std::fs::write(d.path().join("outcome.jsonl.1"), "older").unwrap();
    let line = OutcomeLine {
        schema: 1,
        ts: NOW,
        run_id: "r".into(),
        mode: OutcomeMode::Plan,
        verdict: DiskVerdict::Ok,
        free_bytes: 1,
        unallocated_bytes: None,
        via: Some("v".repeat(500)),
    };
    append_outcome(&sd.paths, None, &line).unwrap();
    assert_eq!(
        std::fs::read(d.path().join("outcome.jsonl.1")).unwrap(),
        pad
    );
    let fresh = std::fs::read_to_string(sd.paths.outcome()).unwrap();
    assert_eq!(fresh.lines().count(), 1);
    assert!(
        fresh.len() <= 201,
        "an overlong via is cut: {}",
        fresh.len()
    );

    // A read-only state dir is an error for the caller to turn into a warning.
    let ro = d.path().join("ro");
    std::fs::create_dir(&ro).unwrap();
    std::fs::set_permissions(&ro, std::os::unix::fs::PermissionsExt::from_mode(0o500)).unwrap();
    if OpenOptions::new()
        .create(true)
        .write(true)
        .open(ro.join("probe"))
        .is_err()
    {
        let paths = disk_cleanup_adapters::state::StatePaths { dir: ro.clone() };
        assert!(append_outcome(&paths, None, &line).is_err());
    }
    std::fs::set_permissions(&ro, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
}

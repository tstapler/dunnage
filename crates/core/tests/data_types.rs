#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::path::PathBuf;

use disk_cleanup_core::*;

fn node_modules_candidate() -> Candidate {
    let path = PathBuf::from("/home/u/code/app/node_modules");
    Candidate::new(
        Locator::Path(path),
        KindFacts::BuildDir(BuildDirFacts {
            flavor: BuildDirFlavor::NodeModules,
            group: PathBuf::from("/home/u/code/app"),
            age_secs: 86_400,
            lockfile_hash: Some(7),
        }),
        AllocatedBytes(4096),
        Some(1_700_000_000),
        Identity { dev: 28, ino: 77 },
    )
}

#[test]
fn audit_record_should_roundtrip_each_line_type_with_a_type_tag() {
    let c = node_modules_candidate();
    let start = AuditRecord::RunStart(RunStart {
        schema: AUDIT_SCHEMA,
        run_id: "r1".into(),
        ts: 5,
        euid: 1000,
        kinds: "caches".into(),
        plan_id: None,
        build: BuildInfo {
            bin_version: "0.1.0".into(),
            git_commit: "abc".into(),
            dirty: false,
            metascope_rev: "def".into(),
            patched: false,
            metascope_dirty: false,
        },
        stamp: Some(StampSummary {
            gate: "A".into(),
            commit: "abc".into(),
            binary_sha256: "00".into(),
            verified: true,
        }),
        break_glass: Some(BreakGlass {
            reason: "disk full".into(),
            tty_confirmed: true,
        }),
        roots: RunRoots::default(),
        verdict_start: None,
    });
    let intent = AuditRecord::Intent(Intent::of(&c, "r1", 6));
    let end = AuditRecord::RunEnd(RunEnd {
        run_id: "r1".into(),
        ts: 7,
        verdict_after: DiskVerdict::Warn,
        attributed_bytes: 4096,
        measured_drop: Some(-1),
        attribution_pct: None,
        explained: false,
        causes: vec![Cause {
            kind: CauseKind::SnapshotHeld,
            bytes: 0,
            source: "unmeasured".into(),
        }],
    });
    for (rec, tag) in [(start, "run_start"), (intent, "intent"), (end, "run_end")] {
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["type"], tag);
        let back: AuditRecord = serde_json::from_value(v).unwrap();
        assert_eq!(back, rec);
    }
}

#[test]
fn item_key_should_cut_on_a_char_boundary_at_200_bytes() {
    let long = "é".repeat(150);
    let k = ItemKey::new(&long);
    assert!(k.as_str().len() <= ITEM_KEY_MAX);
    assert!(k.as_str().chars().all(|c| c == 'é'));
    assert_eq!(ItemKey::new("short").as_str(), "short");
}

#[test]
fn alloc_snapshot_should_report_metadata_headroom() {
    let a = AllocSnapshot {
        data: SpaceUse {
            bytes_used: 1,
            total_bytes: 2,
        },
        metadata: SpaceUse {
            bytes_used: 90,
            total_bytes: 100,
        },
        system: SpaceUse {
            bytes_used: 0,
            total_bytes: 0,
        },
        unallocated: 5,
    };
    assert_eq!(a.metadata_headroom_bytes(), 10);
}

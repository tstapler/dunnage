//! `RunSummary` and `RunSummary::finish`: the attribution math of ADR-004 (Story 1.2.2). Pure: every
//! measurement arrives through [`FinishInput`], so the figures are tested without a filesystem.

use std::collections::HashMap;
use std::fmt::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::kind::Kind;
use crate::outcome::{Outcome, SkipReason, Status};
use crate::ports::{ExtentAttribution, SharedExtent};
use crate::snapshot_types::{FsSnapshot, Held, Settled};

const GIB: f64 = 1_073_741_824.0;
const TARGET_PCT: f64 = 95.0;
/// Above this the counter dropped clearly less than was attributed (the 110 line of Story 2.3.5 sits beyond it).
const EXCEEDS_PCT: f64 = 105.0;
/// Two pre-delete samples closer than this say nothing about another writer's rate.
const MIN_SAMPLE_SPAN_MS: u64 = 2_000;
const CAUSE_TOLERANCE_PCT: i128 = 5;
const SRC_UNMEASURED: &str = "unmeasured";
const SRC_SAMPLES: &str = "two data.bytes_used samples";
const SRC_SNAPSHOT: &str = "snapshot path probe";
const SRC_DOCKER: &str = "docker system df before/after";

/// How the disk stands after a run (the `status` verdict).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskVerdict {
    Ok,
    Warn,
    Crit,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CauseKind {
    SnapshotHeld,
    ConcurrentWriteDelta,
    DockerDaemonDelta,
}

/// A measured explanation for part of the shortfall `attributed - drop`; unmeasured is 0 with source
/// `unmeasured`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cause {
    pub kind: CauseKind,
    pub bytes: i64,
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StageTimings {
    pub discover_ms: u64,
    pub plan_ms: u64,
    pub execute_ms: u64,
    pub settle_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WalkerCounters {
    pub dirs_scanned: u64,
    pub cache_hits: u64,
}

/// The `--json` document body for an apply run. `Executor::run` returns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub fs_before: FsSnapshot,
    /// `None` in plan mode.
    pub fs_after: Option<FsSnapshot>,
    pub outcomes: Vec<Outcome>,
    /// Sum of `freed.attributed_bytes` over every filesystem-kind outcome (failed ones included), with each
    /// shared extent counted once across the run. Docker bytes are daemon-side and never enter it.
    pub attributed_bytes: u64,
    pub measured_drop: Option<i64>,
    /// `measured_drop - attributed_bytes`, signed.
    pub unattributed: Option<i64>,
    /// The settle did not stabilise.
    pub provisional: bool,
    /// The two `data.bytes_used` samples taken before the first delete.
    pub pre_delete_data_bytes_used: Vec<u64>,
    pub causes: Vec<Cause>,
    pub verdict_after: DiskVerdict,
    /// The three largest `KindNotSelected` outcomes.
    pub top_unselected: Vec<Outcome>,
    pub timings: StageTimings,
    pub walker: WalkerCounters,
    pub notes: Vec<String>,
    /// `100 * attributed / (measured_drop - docker_delta)`, uncapped; `None` (printed `n/a`) when that
    /// denominator is not positive.
    pub attribution_pct: Option<f64>,
    /// The `docker system df` delta, clamped to the drop; 0 when unavailable or no Docker item was removed.
    pub docker_delta_bytes: u64,
    /// Shared extents seen once in the removed set: their twin may live outside it, so they may stay on disk.
    pub shared_est_bytes: u64,
    /// True only when measured causes sum to the shortfall within 5% of the drop (two-sided).
    pub explained: bool,
    /// `+3.7 GiB`; `None` without a measured drop.
    pub unattributed_text: Option<String>,
    pub measured_line: String,
    pub attribution_line: String,
    pub followup_line: Option<String>,
    pub docker_line: Option<String>,
    pub shared_line: Option<String>,
    pub snapshot_line: Option<String>,
    /// False when snapshots hold the space: the `Next:` block then offers no wider `--kinds`.
    pub offer_wider_kinds: bool,
}

/// One `data.bytes_used` read taken before the first delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreDeleteSample {
    pub at_ms: u64,
    pub data_bytes_used: u64,
}

/// `SnapshotInfo::holds` for one removed item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldItem {
    pub path: PathBuf,
    pub held: Held,
}

/// Snapshots detected on the filesystem, already formatted for the sentence that names them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotFacts {
    /// `snapper` or `timeshift`.
    pub tool: String,
    /// `#12 2026-09-01`
    pub entries: Vec<String>,
    /// `snapper list`
    pub list_command: String,
}

/// Everything `RunSummary::finish` needs; the executor gathers it, `core` does the arithmetic.
#[derive(Debug, Clone)]
pub struct FinishInput {
    pub fs_before: FsSnapshot,
    pub fs_after: Option<FsSnapshot>,
    pub settled: Option<Settled>,
    pub outcomes: Vec<Outcome>,
    pub extents: ExtentAttribution,
    pub pre_delete: Vec<PreDeleteSample>,
    /// First delete to the end of settle.
    pub delete_window_ms: u64,
    pub held: Vec<HeldItem>,
    pub snapshots: Option<SnapshotFacts>,
    /// `docker system df` before minus after; `None` when unavailable.
    pub docker_delta_bytes: Option<u64>,
    pub verdict_after: DiskVerdict,
    pub timings: StageTimings,
    pub walker: WalkerCounters,
    pub notes: Vec<String>,
}

fn is_docker(k: Kind) -> bool {
    matches!(
        k,
        Kind::DockerImage | Kind::DockerBuildCache | Kind::DockerVolume
    )
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB)
}

fn signed_gib(bytes: i64) -> String {
    let sign = if bytes < 0 { '-' } else { '+' };
    format!("{sign}{:.1} GiB", bytes.unsigned_abs() as f64 / GIB)
}

/// `data.bytes_used` before minus after on btrfs, the `df` used delta elsewhere; `None` without an after.
fn measured_drop(i: &FinishInput) -> Option<(i64, bool)> {
    let after = i.fs_after.as_ref()?;
    let to_i = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    match (&i.fs_before.btrfs, &after.btrfs) {
        (Some(b), a) => {
            let after_used = i
                .settled
                .as_ref()
                .and_then(|s| s.data_bytes_used)
                .or_else(|| a.as_ref().map(|a| a.data.bytes_used))?;
            Some((to_i(b.data.bytes_used) - to_i(after_used), true))
        }
        (None, _) => Some((to_i(i.fs_before.used_bytes) - to_i(after.used_bytes), false)),
    }
}

/// Sum of the removed items' `st_blocks` bytes minus every shared extent an earlier item already counted,
/// and the bytes of shared extents that appear only once in the set.
fn attribute(outcomes: &[Outcome], extents: &ExtentAttribution) -> (u64, u64) {
    let fs_items = || outcomes.iter().filter(|o| !is_docker(o.kind));
    let plain: u64 = fs_items().map(|o| o.freed.attributed_bytes.0).sum();
    let ExtentAttribution::Measured(items) = extents else {
        return (plain, 0);
    };
    let by_path: HashMap<&PathBuf, &Vec<SharedExtent>> =
        items.iter().map(|e| (&e.path, &e.shared)).collect();
    let mut seen: HashMap<SharedExtent, u32> = HashMap::new();
    let mut duplicate = 0_u64;
    for o in fs_items() {
        let Some(shared) = o.path.as_ref().and_then(|p| by_path.get(p)) else {
            continue;
        };
        for e in *shared {
            let n = seen.entry(*e).or_insert(0);
            if *n > 0 {
                duplicate = duplicate.saturating_add(e.length);
            }
            *n += 1;
        }
    }
    let once_only = seen
        .iter()
        .filter(|(_, n)| **n == 1)
        .map(|(e, _)| e.length)
        .sum();
    (plain.saturating_sub(duplicate), once_only)
}

fn concurrent_write_cause(i: &FinishInput) -> Cause {
    let measured = match (i.pre_delete.first(), i.pre_delete.last()) {
        (Some(a), Some(b)) if b.at_ms.saturating_sub(a.at_ms) >= MIN_SAMPLE_SPAN_MS => {
            let span = i128::from(b.at_ms - a.at_ms);
            let growth = i128::from(b.data_bytes_used) - i128::from(a.data_bytes_used);
            let bytes = growth * i128::from(i.delete_window_ms) / span;
            Some(i64::try_from(bytes).unwrap_or(0))
        }
        _ => None,
    };
    Cause {
        kind: CauseKind::ConcurrentWriteDelta,
        bytes: measured.unwrap_or(0),
        source: if measured.is_some() {
            SRC_SAMPLES
        } else {
            SRC_UNMEASURED
        }
        .to_string(),
    }
}

/// The measured bytes of the removed items that a snapshot still holds: an upper bound, so it is a cause
/// only because a path probe found them.
fn snapshot_cause(i: &FinishInput) -> Cause {
    let bytes: u64 = i
        .held
        .iter()
        .filter(|h| h.held == Held::Found)
        .filter_map(|h| {
            i.outcomes
                .iter()
                .find(|o| o.path.as_ref() == Some(&h.path) && o.status == Status::Removed)
        })
        .map(|o| o.measured_bytes.unwrap_or(o.freed.attributed_bytes.0))
        .sum();
    Cause {
        kind: CauseKind::SnapshotHeld,
        bytes: i64::try_from(bytes).unwrap_or(i64::MAX),
        source: if bytes > 0 {
            SRC_SNAPSHOT
        } else {
            SRC_UNMEASURED
        }
        .to_string(),
    }
}

impl RunSummary {
    /// Build the run's figures and the lines that state them (UX-10, UX-11; ADR-004).
    #[must_use]
    pub fn finish(input: FinishInput) -> RunSummary {
        let (attributed, shared_est) = attribute(&input.outcomes, &input.extents);
        let docker_removed = input
            .outcomes
            .iter()
            .any(|o| is_docker(o.kind) && o.status == Status::Removed);
        let drop = measured_drop(&input);
        let measured = drop.map(|(d, _)| d);
        let on_btrfs = drop.is_none_or(|(_, b)| b);
        let provisional = input.settled.as_ref().is_some_and(|s| !s.stable);
        let drop_pos = measured.map_or(0, |d| u64::try_from(d).unwrap_or(0));
        let docker_delta = if docker_removed {
            input.docker_delta_bytes.unwrap_or(0).min(drop_pos)
        } else {
            0
        };
        let denominator = measured.map(|d| d - i64::try_from(docker_delta).unwrap_or(0));
        let pct = denominator
            .filter(|d| *d > 0)
            .map(|d| 100.0 * attributed as f64 / d as f64);
        let unattributed = measured.map(|d| d - i64::try_from(attributed).unwrap_or(i64::MAX));

        let mut causes = vec![concurrent_write_cause(&input), snapshot_cause(&input)];
        if docker_removed {
            causes.push(Cause {
                kind: CauseKind::DockerDaemonDelta,
                bytes: i64::try_from(docker_delta).unwrap_or(0),
                source: if input.docker_delta_bytes.is_some() {
                    SRC_DOCKER
                } else {
                    SRC_UNMEASURED
                }
                .to_string(),
            });
        }
        let explained = measured.zip(denominator).is_some_and(|(d, net)| {
            // The Docker cause is already netted out of `net`, so it stays out of the sum.
            let sum: i128 = causes
                .iter()
                .filter(|c| c.kind != CauseKind::DockerDaemonDelta)
                .map(|c| i128::from(c.bytes))
                .sum();
            let shortfall = i128::from(attributed) - i128::from(net);
            let any_measured = causes
                .iter()
                .any(|c| c.kind != CauseKind::DockerDaemonDelta && c.source != SRC_UNMEASURED);
            let tolerance = i128::from(d.max(0)) * CAUSE_TOLERANCE_PCT / 100;
            any_measured && (sum - shortfall).abs() <= tolerance
        });

        let snapshot_line = input.snapshots.as_ref().filter(|s| !s.entries.is_empty()).and_then(|s| {
            (attributed > 0 && drop_pos < attributed / 10).then(|| {
                let n = s.entries.len();
                let plural = if n == 1 { "snapshot" } else { "snapshots" };
                format!(
                    "Only {} of {} came back: {n} {plural} ({} {}) can still hold the deleted extents. \
                     List them with '{}' and delete old ones yourself; do not widen --kinds.",
                    gib(drop_pos),
                    gib(attributed),
                    s.tool,
                    s.entries.join(", "),
                    s.list_command,
                )
            })
        });

        let mut notes = input.notes.clone();
        if attributed > 0 && matches!(input.extents, ExtentAttribution::Unavailable(_)) {
            notes.push(
                "shared extents not measured here: attribution may exceed the drop".to_string(),
            );
        }

        let measured_line = measured_line(measured, on_btrfs, provisional, &input);
        let attribution_line = attribution_line(pct, provisional, docker_delta > 0, docker_removed);
        let mut top_unselected: Vec<Outcome> = input
            .outcomes
            .iter()
            .filter(|o| o.status == Status::Skipped(SkipReason::KindNotSelected))
            .cloned()
            .collect();
        top_unselected.sort_by_key(|o| std::cmp::Reverse(o.est_bytes));
        top_unselected.truncate(3);

        RunSummary {
            fs_before: input.fs_before,
            fs_after: input.fs_after,
            outcomes: input.outcomes,
            attributed_bytes: attributed,
            measured_drop: measured,
            unattributed,
            provisional,
            pre_delete_data_bytes_used: input
                .pre_delete
                .iter()
                .map(|s| s.data_bytes_used)
                .collect(),
            causes,
            verdict_after: input.verdict_after,
            top_unselected,
            timings: input.timings,
            walker: input.walker,
            notes,
            attribution_pct: pct,
            docker_delta_bytes: docker_delta,
            shared_est_bytes: shared_est,
            explained,
            unattributed_text: unattributed.map(signed_gib),
            measured_line,
            attribution_line,
            followup_line: provisional.then(|| {
                "Run 'disk-cleanup status' in a minute for the settled figure".to_string()
            }),
            docker_line: (docker_removed && docker_delta > 0).then(|| {
                format!(
                    "docker: {} (daemon-side; not in btrfs terms)",
                    gib(docker_delta)
                )
            }),
            shared_line: (shared_est > 0)
                .then(|| format!("shared extents (may stay on disk): {}", gib(shared_est))),
            offer_wider_kinds: snapshot_line.is_none(),
            snapshot_line,
        }
    }
}

fn measured_line(
    measured: Option<i64>,
    on_btrfs: bool,
    provisional: bool,
    i: &FinishInput,
) -> String {
    let what = if on_btrfs { "data" } else { "df" };
    let Some(d) = measured else {
        return format!("Measured drop in {what} used n/a");
    };
    let mut line = format!("Measured drop in {what} used {}", signed_unless_positive(d));
    if provisional {
        let secs = i.settled.as_ref().map_or(0, |s| (s.waited_ms + 500) / 1000);
        let _ = write!(
            line,
            " (provisional: counter was still falling after {secs}s)"
        );
    } else if !on_btrfs {
        let _ = write!(
            line,
            " (df used delta; filesystem is {})",
            i.fs_before.fs_type
        );
    }
    line
}

fn signed_unless_positive(d: i64) -> String {
    if d < 0 {
        signed_gib(d)
    } else {
        gib(d.unsigned_abs())
    }
}

fn attribution_line(
    pct: Option<f64>,
    provisional: bool,
    docker_excluded: bool,
    docker_items: bool,
) -> String {
    let Some(p) = pct else {
        return "Attribution n/a (no measured drop to compare with)".to_string();
    };
    let lead = if provisional { "provisional; " } else { "" };
    if docker_excluded && p >= TARGET_PCT {
        return format!(
            "Attribution {p:.1}% over filesystem kinds ({lead}target >= 95%; docker excluded)"
        );
    }
    if p > EXCEEDS_PCT {
        return format!(
            "Attribution {p:.1}% ({lead}attributed exceeds the counter drop: concurrent writes or shared extents)"
        );
    }
    if p < TARGET_PCT {
        let why = if docker_items && !docker_excluded {
            "; docker figure unavailable, so its bytes stay in the remainder"
        } else {
            ""
        };
        let tail = if why.is_empty() {
            " - see unattributed remainder"
        } else {
            ""
        };
        return format!("Attribution {p:.1}% ({lead}below target{why}){tail}");
    }
    format!("Attribution {p:.1}% ({lead}target >= 95%)")
}

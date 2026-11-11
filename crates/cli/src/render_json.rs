//! The JSON documents of `design/ux.md` S7: plan, apply, `status`, `kinds` and the fatal and gate-refusal
//! documents. Each is exactly one document with `schema_version`, `mode`, `exit_code` and `errors[]`; bytes
//! are exact integers, paths are absolute, and nothing is coloured, truncated or locale-formatted.

use std::path::Path;

use disk_cleanup_core::kind::SelectorId;
use disk_cleanup_core::outcome::SkipReason;
use disk_cleanup_core::plan::{ChangeCause, GateRefusal, GateRenderCtx, BIN};
use disk_cleanup_core::reasons::FatalError;
use disk_cleanup_core::selector::{default_min_age_secs, format_age, KindSelector};
use disk_cleanup_core::snapshot_types::FsSnapshot;
use disk_cleanup_core::summary::{DiskVerdict, RunSummary};
use serde_json::{json, Map, Value};

use crate::apply::{apply_rows, freed_total, ApplyInput};
use crate::escape::path_hex;
use crate::view::{family, ErrorEntry, FsBlock, ItemRow, PlanInput, PlanView, RowStatus};

pub const SCHEMA_VERSION: u32 = 1;

/// The one-liner `--help` prints for reading the report by column.
pub const JQ_TSV_ONE_LINER: &str =
    "disk-cleanup --json | jq -r '.items[] | [.status, .kind, .est_bytes, .path] | @tsv'";

/// One document, pretty-printed, ending in a newline.
pub fn to_text(doc: &Value) -> String {
    let mut s = serde_json::to_string_pretty(doc).unwrap_or_else(|_| "{}".to_string());
    s.push('\n');
    s
}

fn lossy(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn item_json(r: &ItemRow) -> Value {
    let mut m = Map::new();
    m.insert("kind".into(), json!(r.kind.id()));
    m.insert("selector".into(), json!(r.selector.id()));
    m.insert("family".into(), json!(family(r.kind)));
    m.insert("id".into(), json!(r.id.as_hex()));
    m.insert("path".into(), json!(r.path.as_deref().map(lossy)));
    if r.path.is_none() {
        m.insert("name".into(), json!(r.label.to_string_lossy()));
    }
    if let Some(hex) = path_hex(r.path.as_deref().map_or(&r.label, |p| p.as_os_str())) {
        m.insert("path_hex".into(), json!(hex));
    }
    m.insert("tier".into(), json!(tier_id(r)));
    m.insert("status".into(), json!(r.status.json()));
    m.insert("est_bytes".into(), json!(r.est_bytes));
    m.insert("measured_bytes".into(), json!(r.measured_bytes));
    m.insert("elapsed_ms".into(), json!(r.elapsed_ms));
    m.insert("reason".into(), json!(r.reason.map(reason_id)));
    m.insert("reason_text".into(), json!(r.reason_text));
    m.insert("next_action".into(), json!(r.next_action));
    if let Some(e) = &r.error_text {
        m.insert("error".into(), json!(e));
    }
    Value::Object(m)
}

fn tier_id(r: &ItemRow) -> &'static str {
    match r.tier() {
        disk_cleanup_core::kind::Risk::Safe => "safe",
        disk_cleanup_core::kind::Risk::Guarded => "guarded",
        disk_cleanup_core::kind::Risk::Destructive => "destructive",
    }
}

/// The kebab-case id of a skip reason, as the audit log and `reason` carry it.
pub fn reason_id(r: SkipReason) -> String {
    serde_json::to_value(r)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn snapshot_json(s: &FsSnapshot) -> Value {
    let btrfs = s.btrfs.map_or_else(
        || json!({}),
        |b| {
            json!({
                "data": {"bytes_used": b.data.bytes_used, "total_bytes": b.data.total_bytes},
                "metadata": {"bytes_used": b.metadata.bytes_used, "total_bytes": b.metadata.total_bytes},
                "system": {"bytes_used": b.system.bytes_used, "total_bytes": b.system.total_bytes},
                "unallocated": b.unallocated,
            })
        },
    );
    json!({
        "df": {
            "path": lossy(&s.path),
            "fs_type": s.fs_type,
            "total_bytes": s.total_bytes,
            "used_bytes": s.used_bytes,
            "avail_bytes": s.avail_bytes,
            "taken_at": s.taken_at,
        },
        "btrfs": btrfs,
    })
}

pub fn verdict_id(v: DiskVerdict) -> &'static str {
    match v {
        DiskVerdict::Ok => "ok",
        DiskVerdict::Warn => "warn",
        DiskVerdict::Crit => "crit",
        DiskVerdict::Unknown => "unknown",
    }
}

fn errors_json(errors: &[ErrorEntry]) -> Value {
    Value::Array(
        errors
            .iter()
            .map(|e| {
                json!({
                    "code": e.code,
                    "path": e.path.as_deref().map(lossy),
                    "message": e.message,
                })
            })
            .collect(),
    )
}

/// The stable lower-case code of an `ERR` row's text.
pub fn error_code(text: &str) -> &'static str {
    if text.starts_with("permission denied") {
        "eacces"
    } else if text.starts_with("no space left") {
        "enospc"
    } else if text.starts_with("command") {
        "command_failed"
    } else {
        "io_error"
    }
}

fn row_errors(rows: &[ItemRow]) -> Vec<ErrorEntry> {
    rows.iter()
        .filter(|r| r.status == RowStatus::Err)
        .map(|r| {
            let text = r.error_text.clone().unwrap_or_default();
            ErrorEntry {
                code: error_code(&text).into(),
                path: r.path.clone(),
                message: text,
            }
        })
        .collect()
}

/// The plan document (`mode: "plan"`).
pub fn plan_doc(input: &PlanInput, view: &PlanView, exit_code: i32) -> Value {
    let mut notes: Vec<String> = input.notes.clone();
    if !input.not_built.is_empty() {
        notes.push(format!(
            "not available in this build: {}",
            input.not_built.join(", ")
        ));
    }
    notes.extend(input.wrapper_note.clone());
    let next: Vec<Value> = view
        .next
        .iter()
        .map(|c| json!({"command": c.command, "plan_id": c.plan_id.map(|i| i.to_string()), "explain": c.explain}))
        .collect();
    json!({
        "schema_version": SCHEMA_VERSION,
        "mode": "plan",
        "exit_code": exit_code,
        "plan_id": view.header_id.to_string(),
        "plan_saved": view.plan_saved,
        "partial_scan": input.partial_scan.is_some(),
        "items": view.rows.iter().map(item_json).collect::<Vec<_>>(),
        "totals": {
            "estimated_bytes": view.selected.bytes,
            "attributed_bytes": 0,
            "measured_drop_bytes": null,
            "unattributed_bytes": null,
            "attribution_pct": null,
        },
        "next": next,
        "fs": {
            "before": input.fs.as_ref().map(|f| snapshot_json(&f.snapshot)),
            "after": null,
        },
        "notes": notes,
        "errors": errors_json(&input.errors),
    })
}

fn summary_json(s: &RunSummary) -> Value {
    json!({
        "provisional": s.provisional,
        "verdict_after": verdict_id(s.verdict_after),
        "docker_delta_bytes": s.docker_delta_bytes,
        "shared_est_bytes": s.shared_est_bytes,
        "explained": s.explained,
        "causes": s.causes.iter().map(|c| json!({
            "kind": serde_json::to_value(c.kind).unwrap_or(Value::Null),
            "bytes": c.bytes,
            "source": c.source,
        })).collect::<Vec<_>>(),
        "measured_line": s.measured_line,
        "attribution_line": s.attribution_line,
        "timings": {
            "discover_ms": s.timings.discover_ms,
            "plan_ms": s.timings.plan_ms,
            "execute_ms": s.timings.execute_ms,
            "settle_ms": s.timings.settle_ms,
        },
    })
}

/// The apply document (`mode: "apply"`).
pub fn apply_doc(input: &ApplyInput) -> Value {
    let s = input.summary;
    let rows = apply_rows(input);
    let estimated: u64 = rows
        .iter()
        .filter(|r| r.status != RowStatus::Risk)
        .map(|r| r.est_bytes)
        .sum();
    json!({
        "schema_version": SCHEMA_VERSION,
        "mode": "apply",
        "exit_code": input.exit_code,
        "plan_id": input.plan_id.to_string(),
        "partial_scan": false,
        "items": rows.iter().map(item_json).collect::<Vec<_>>(),
        "totals": {
            "estimated_bytes": estimated,
            "freed_bytes": freed_total(s),
            "attributed_bytes": s.attributed_bytes,
            "measured_drop_bytes": s.measured_drop,
            "unattributed_bytes": s.unattributed,
            "attribution_pct": s.attribution_pct,
        },
        "summary": summary_json(s),
        "next": [],
        "fs": {
            "before": snapshot_json(&s.fs_before),
            "after": s.fs_after.as_ref().map(snapshot_json),
        },
        "notes": s.notes,
        "errors": errors_json(&row_errors(&rows)),
    })
}

/// The `status` document.
pub fn status_doc(
    path: &Path,
    fs: Option<&FsBlock>,
    errors: &[ErrorEntry],
    exit_code: i32,
) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "mode": "status",
        "exit_code": exit_code,
        "path": lossy(path),
        "verdict": verdict_id(fs.map_or(DiskVerdict::Unknown, |f| f.verdict)),
        "fs": {"before": fs.map(|f| snapshot_json(&f.snapshot)), "after": null},
        "notes": [],
        "errors": errors_json(errors),
    })
}

/// One row of `kinds` (S5).
#[derive(Debug, Clone)]
pub struct KindRow {
    pub id: &'static str,
    pub family: &'static str,
    pub tier: &'static str,
    pub default: bool,
    /// `yes`, `needs root`, `daemon down`, `not available in this build`, ... (`KindGate::available_label`).
    pub available: String,
    pub min_age: Option<String>,
    pub touches: &'static str,
}

fn touches(s: SelectorId) -> &'static str {
    match s {
        SelectorId::Caches => "npm, go, gradle, pip, cargo, playwright",
        SelectorId::CacheApt => "/var/cache/apt",
        SelectorId::Worktrees => ".claude/worktrees/agent-*",
        SelectorId::BuildDirs => "node_modules, cargo target",
        SelectorId::Docker => "dangling images, build cache",
        SelectorId::DockerImages => "unused tagged images pulled from a registry (locally built ones are skipped, LocalOnlyImage)",
        SelectorId::DockerVolumes => "unreferenced volumes (--docker-volumes anonymous|named)",
        SelectorId::Steam => "orphan prefixes, shader caches, uninstalled-game remnants",
        SelectorId::Btrfs => "balance (usage ladder)",
        SelectorId::BtrfsTrim => "fstrim",
    }
}

/// The `kinds` table, one row per selector in id order; `available` is the AVAILABLE cell per selector.
pub fn kind_rows(available: &dyn Fn(SelectorId) -> String) -> Vec<KindRow> {
    let defaults = KindSelector::default_apply();
    SelectorId::ALL
        .iter()
        .map(|&s| KindRow {
            id: s.id(),
            family: family(s.kinds()[0]),
            tier: match s.tier() {
                disk_cleanup_core::kind::Risk::Safe => "safe",
                disk_cleanup_core::kind::Risk::Guarded => "guarded",
                disk_cleanup_core::kind::Risk::Destructive => "destructive",
            },
            default: defaults.contains(s) || s == SelectorId::CacheApt,
            available: available(s),
            min_age: default_min_age_secs(s).map(format_age),
            touches: touches(s),
        })
        .collect()
}

/// The `kinds` document.
pub fn kinds_doc(rows: &[KindRow]) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "mode": "kinds",
        "exit_code": 0,
        "kinds": rows.iter().map(|r| json!({
            "id": r.id, "family": r.family, "tier": r.tier, "default": r.default,
            "available": r.available, "min_age": r.min_age, "touches": r.touches,
        })).collect::<Vec<_>>(),
        "errors": [],
    })
}

fn fatal_code(e: &FatalError) -> &'static str {
    match e {
        FatalError::LockHeld { .. } => "lock_held",
        FatalError::AuditUnwritable { .. } => "audit_unwritable",
        FatalError::RootsUnreadable { .. } => "roots_unreadable",
        FatalError::Starved { .. } => "starved",
        FatalError::StateUnreadable { .. } => "state_unreadable",
    }
}

/// The `errors[]` element of a run-level error.
pub fn fatal_entry(e: &FatalError) -> ErrorEntry {
    let path = match e {
        FatalError::AuditUnwritable { path, .. }
        | FatalError::RootsUnreadable { path, .. }
        | FatalError::StateUnreadable { path, .. } => Some(path.clone()),
        _ => None,
    };
    ErrorEntry {
        code: fatal_code(e).into(),
        path,
        message: e.text(),
    }
}

/// A fatal runtime error: the document of the mode with `items` as far as known and `errors[]` filled.
pub fn fatal_doc(mode: &str, plan_id: Option<&str>, e: &FatalError) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "mode": mode,
        "exit_code": e.exit_code(),
        "plan_id": plan_id,
        "partial_scan": false,
        "items": [],
        "next": [],
        "notes": [],
        "errors": errors_json(&[fatal_entry(e)]),
    })
}

/// A gate refusal under `--json` is data: one document with the refusal in `errors[]` (UX-55).
pub fn gate_refusal_doc(r: &GateRefusal, ctx: &GateRenderCtx) -> Value {
    let recent_of = |v: &[disk_cleanup_core::plan::RecentPlan]| -> Value {
        v.iter()
            .map(|p| json!({"plan_id": p.plan_id, "args": p.args, "age_secs": p.age_secs}))
            .collect()
    };
    let dry = format!("{BIN} {}", ctx.selection.dry_run_args());
    let (code, supplied, detail) = match r {
        GateRefusal::MissingPlanId { recent } => (
            "plan_id_missing",
            None,
            json!({"recent": recent_of(recent), "dry_run_command": dry}),
        ),
        GateRefusal::NoRecentPlan { supplied, recent } => (
            "plan_id_unknown",
            Some(supplied.to_string()),
            json!({"recent": recent_of(recent), "dry_run_command": dry}),
        ),
        GateRefusal::Stale {
            supplied,
            age_secs,
            recent,
        } => (
            "plan_id_stale",
            Some(supplied.to_string()),
            json!({"age_secs": age_secs, "recent": recent_of(recent), "dry_run_command": dry}),
        ),
        GateRefusal::Changed {
            supplied,
            recomputed,
            cause,
        } => {
            let (code, d) = match cause {
                ChangeCause::ItemsMoved {
                    vanished,
                    appeared,
                    incomplete,
                } => (
                    "plan_id_changed",
                    json!({
                        "supplied": supplied.to_string(),
                        "recomputed": recomputed.to_string(),
                        "vanished": vanished.iter().map(disk_cleanup_core::ItemKey::as_str).collect::<Vec<_>>(),
                        "appeared": appeared.iter().map(disk_cleanup_core::ItemKey::as_str).collect::<Vec<_>>(),
                        "incomplete": incomplete,
                        "dry_run_command": dry,
                    }),
                ),
                ChangeCause::SelectionDiffers {
                    saved_kinds,
                    saved_flags,
                    now_kinds,
                    now_flags,
                } => (
                    "plan_id_selection_differs",
                    json!({
                        "supplied": supplied.to_string(),
                        "recomputed": recomputed.to_string(),
                        "saved": {"kinds": saved_kinds, "flags": saved_flags},
                        "now": {"kinds": now_kinds, "flags": now_flags},
                        "dry_run_command": dry,
                    }),
                ),
            };
            (code, Some(supplied.to_string()), d)
        }
    };
    json!({
        "schema_version": SCHEMA_VERSION,
        "mode": "apply",
        "exit_code": 64,
        "plan_id": supplied,
        "items": [],
        "errors": [{"code": code, "message": r.render(ctx), "detail": detail}],
    })
}

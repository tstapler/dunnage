//! `explore --json` (Story 6.5.5): one document built from the same [`Model`] as the text view, so the
//! numbers cannot disagree. Integers are exact; additive keys never bump `schema_version`.

use std::io::Write;

use serde_json::{json, Value};

use dunnage_core::escape::path_hex;

use super::text::{
    build_model, scan_to_completion, Entry, ExploreEnv, Model, ScanEnd, Scanned, INTERRUPTED,
};
use crate::render_json::SCHEMA_VERSION;

fn path_keys(p: &std::path::Path) -> (String, Option<String>) {
    (p.to_string_lossy().into_owned(), path_hex(p.as_os_str()))
}

fn entry_json(e: &Entry) -> Value {
    let (path, hex) = path_keys(&e.path);
    let share = if e.parent_usage == 0 {
        Value::Null
    } else {
        json!(e.usage.min(e.parent_usage) as f64 / e.parent_usage as f64)
    };
    let mut v = json!({
        "path": path,
        "usage_bytes": e.usage,
        "apparent_bytes": e.apparent,
        "meta_est_bytes": e.meta,
        "shared_est_bytes": null,
        "newest_dir_mtime": e.newest_mtime,
        "items": e.items,
        "share_of_parent": share,
        "state": e.state.json(),
        "kind": e.kind,
        "errors": e.errors.iter().map(|n| json!({"errno": n})).collect::<Vec<_>>(),
    });
    if let Some(h) = hex {
        v["path_hex"] = json!(h);
    }
    if let Some(s) = e.synthetic {
        v["synthetic"] = json!(s);
        v["name"] = json!(e.name);
    }
    v
}

/// The document for a finished scan.
pub fn document(env: &ExploreEnv, s: &Scanned, model: &Model) -> Value {
    let fs = match &env.headroom {
        Ok(h) => {
            let (verdict, _) = dunnage_adapters::status::assess(h);
            let verdict = crate::render::verdict_text(verdict)
                .trim_start_matches("verdict: ")
                .to_lowercase();
            json!({
                "verdict": verdict,
                "avail_bytes": h.avail_bytes,
                "unallocated_bytes": h.btrfs.map(|a| a.unallocated),
                "data_slack_bytes": h.btrfs.map(|a| a.data.headroom_bytes()),
                "meta_slack_bytes": h.btrfs.map(|a| a.metadata_headroom_bytes()),
            })
        }
        Err(_) => json!({
            "verdict": "unknown",
            "avail_bytes": null,
            "unallocated_bytes": null,
            "data_slack_bytes": null,
            "meta_slack_bytes": null,
        }),
    };
    let entries: Vec<Value> = model
        .roots
        .iter()
        .flat_map(|r| r.entries.iter().map(entry_json))
        .collect();
    let roots: Vec<Value> = model
        .roots
        .iter()
        .map(|r| {
            let (path, hex) = path_keys(&r.path);
            let mut v = json!({
                "path": path,
                "usage_bytes": r.usage,
                "apparent_bytes": r.apparent,
                "items": r.items,
                "at_least": r.at_least,
            });
            if let Some(h) = hex {
                v["path_hex"] = json!(h);
            }
            v
        })
        .collect();
    json!({
        "schema_version": SCHEMA_VERSION,
        "mode": "explore",
        "partial": s.summary.truncated,
        "roots": roots,
        "filesystem": fs,
        "entries": entries,
        "exit_code": 0,
        "errors": [],
    })
}

/// Runs the scan and writes the one document; the exit value is 0 (also for a partial scan).
pub fn run_json(env: &ExploreEnv, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let scanned = match scan_to_completion(env) {
        ScanEnd::Interrupted(code) => {
            let _ = writeln!(err, "{INTERRUPTED}");
            return code;
        }
        ScanEnd::Done(s) => *s,
    };
    let model = build_model(env, &scanned);
    let doc = document(env, &scanned, &model);
    let _ = out.write_all(crate::render_json::to_text(&doc).as_bytes());
    let _ = out.flush();
    0
}

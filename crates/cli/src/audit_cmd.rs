//! `disk-cleanup audit verify [--log PATH] [--json]` (Story 1.2.4, Task 1.2.4d). Read-only: no lock, no audit
//! write, no state write. Exit 0 clean, 2 a deletion outside the allowed roots, 74 log unreadable.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use disk_cleanup_adapters::audit_verify::{verify_file, AllowedTable, VerifyInputs, VerifyReport};
use serde_json::json;

use crate::render_json::{to_text, SCHEMA_VERSION};

/// What the subcommand printed.
#[derive(Debug)]
pub struct AuditRun {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

fn document(report: &VerifyReport, code: i32) -> String {
    to_text(&json!({
        "schema_version": SCHEMA_VERSION,
        "mode": "audit-verify",
        "exit_code": code,
        "deletions": report.deletions,
        "runs": report.runs,
        "violations": report.violations.iter().map(|v| json!({
            "run_id": v.run_id, "kind": v.kind, "path": v.path, "reason": v.reason,
        })).collect::<Vec<_>>(),
        "unfinished_runs": report.unfinished,
        "warnings": report.warnings,
        "errors": [],
    }))
}

pub fn run(log: &Path, home: &Path, config_roots: Option<&[PathBuf]>, json: bool) -> AuditRun {
    let table = AllowedTable::embedded();
    let inputs = VerifyInputs {
        table: &table,
        home,
        config_roots,
    };
    let report = match verify_file(log, &inputs) {
        Ok(r) => r,
        Err(e) => {
            let text = format!(
                "error: cannot read the audit log {}: {}. Check that the file exists and is readable, or pass --log PATH.\n",
                log.display(),
                e.reason()
            );
            let stdout = if json {
                to_text(&json!({
                    "schema_version": SCHEMA_VERSION,
                    "mode": "audit-verify",
                    "exit_code": 74,
                    "errors": [{"code": "audit_unreadable", "path": log.to_string_lossy(), "message": text.trim_end()}],
                }))
            } else {
                String::new()
            };
            return AuditRun {
                code: 74,
                stdout,
                stderr: text,
            };
        }
    };
    let code = report.exit_code();
    if json {
        return AuditRun {
            code,
            stdout: document(&report, code),
            stderr: String::new(),
        };
    }
    let mut out = String::new();
    for v in &report.violations {
        let _ = writeln!(out, "run {} {} {}: {}", v.run_id, v.kind, v.path, v.reason);
    }
    for r in &report.unfinished {
        let _ = writeln!(
            out,
            "run {r} unfinished: it has no run_end (killed or power loss)"
        );
    }
    for w in &report.warnings {
        let _ = writeln!(out, "warning: {w}");
    }
    out.push_str(&report.summary_line());
    out.push('\n');
    AuditRun {
        code,
        stdout: out,
        stderr: String::new(),
    }
}

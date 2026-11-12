//! The explorer's checks of `audit verify` (Task 6.6.3c, ADR-006 A7, review 2 N6).
//!
//! The rule is code, not a row of the owner-pinned `ci/allowed-roots.txt` (which would need a re-pin by the
//! owner for no gain: the table lists the paths a cleaner kind may touch, and this kind's paths are
//! arbitrary). A path of kind `explorer-selection` is inside only if it is
//!
//! - lexically clean and absolute,
//! - strictly below a root of the reviewed table's defaults or the owner's `config.toml` (never a root the
//!   run recorded about itself),
//! - without a `.git` component,
//! - not `$HOME` or an ancestor of it, and at least two components below `$HOME` when it is under it,
//! - not inside, equal to or above the tool's own state, cache or config directory (read at verify time).
//!
//! Per run: exactly one `explorer_confirm` whose `typed` equals `count` and whose item list has `count`
//! entries; every `intent` and `result` names a listed path; a finished run has `intents + skips == count`; a
//! refused run has no `intent`; an interrupted or unfinished run has `intents + skips <= count`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::audit_verify::{
    components, line_path, show, under, unhex, VerifyExtras, VerifyInputs, VerifyReport, Violation,
};

pub(crate) const KIND: &str = "explorer-selection";

fn bytes_of(p: &std::path::Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

/// `None` when `path` is inside the allowed set, else why not.
pub(crate) fn judge(path: &[u8], inp: &VerifyInputs, extras: &VerifyExtras) -> Option<String> {
    let Some(p) = components(path) else {
        return Some(
            "path is relative or not lexically clean (a '..', '.', or empty component)".into(),
        );
    };
    if p.iter().any(|c| *c == b".git") {
        return Some("a .git component: an explorer delete never enters a repository".into());
    }
    let roots = inp.trusted_roots();
    let below_a_root = roots.iter().any(|r| {
        components(r)
            .and_then(|root| under(&p, &root).map(|rel| !rel.is_empty()))
            .unwrap_or(false)
    });
    if !below_a_root {
        return Some(
            "not strictly below a root of the reviewed table or your config; if intended, add the root to your config"
                .into(),
        );
    }
    let home_bytes = bytes_of(inp.home);
    let home = components(&home_bytes).unwrap_or_default();
    if p.starts_with(&home) {
        let below_home = p.len() - home.len();
        if below_home < 2 {
            return Some(
                "$HOME itself or fewer than 2 components below it: rule F of the delete engine"
                    .into(),
            );
        }
    } else if home.starts_with(&p) {
        return Some("an ancestor of $HOME: rule F of the delete engine".into());
    }
    for own in &extras.own_dirs {
        let own_bytes = bytes_of(own);
        let Some(o) = components(&own_bytes) else {
            continue;
        };
        if p.starts_with(&o) || o.starts_with(&p) {
            return Some("the tool's own state, cache or config directory (rule M5)".into());
        }
    }
    None
}

#[derive(Default)]
struct Confirm {
    count: u64,
    typed: String,
    /// `None` when the line carries no item list or an item path is not valid hex.
    items: Vec<Vec<u8>>,
    items_ok: bool,
}

#[derive(Default)]
struct Run {
    explorer: bool,
    confirms: Vec<Confirm>,
    intents: u64,
    skips: u64,
    results: u64,
    paths: Vec<(&'static str, Vec<u8>)>,
    ended: bool,
    interrupted: bool,
    refused: Option<String>,
}

/// Bookkeeping of the explorer runs seen in one log; judged at the end of the log.
#[derive(Default)]
pub(crate) struct ExplorerBook {
    runs: BTreeMap<String, Run>,
    order: Vec<String>,
}

impl ExplorerBook {
    fn run(&mut self, id: &str) -> &mut Run {
        if !self.runs.contains_key(id) {
            self.order.push(id.to_string());
        }
        self.runs.entry(id.to_string()).or_default()
    }

    pub(crate) fn run_start(&mut self, id: &str, v: &Value) {
        let explorer = v.get("kinds").and_then(Value::as_str) == Some(KIND);
        self.run(id).explorer = explorer;
    }

    pub(crate) fn run_end(&mut self, id: &str, v: &Value) {
        let r = self.run(id);
        r.ended = true;
        r.interrupted = v.get("interrupted").and_then(Value::as_bool) == Some(true);
        r.refused = v.get("refused").and_then(Value::as_str).map(str::to_string);
    }

    pub(crate) fn confirm(&mut self, id: &str, v: &Value, report: &mut VerifyReport) {
        let mut c = Confirm {
            count: v.get("count").and_then(Value::as_u64).unwrap_or(u64::MAX),
            typed: v
                .get("typed")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            items_ok: true,
            ..Confirm::default()
        };
        match v.get("items").and_then(Value::as_array) {
            None => c.items_ok = false,
            Some(items) => {
                for item in items {
                    let path = item.get("path").and_then(Value::as_str).unwrap_or("");
                    let bytes = match item.get("path_hex").and_then(Value::as_str) {
                        None => Some(path.as_bytes().to_vec()),
                        Some(h) => unhex(h).filter(|b| String::from_utf8_lossy(b) == path),
                    };
                    match bytes {
                        Some(b) => c.items.push(b),
                        None => {
                            c.items_ok = false;
                            report.violations.push(Violation {
                                run_id: id.to_string(),
                                kind: KIND.to_string(),
                                path: path.to_string(),
                                reason: "explorer_confirm: path_hex is not valid hex or differs from path"
                                    .into(),
                            });
                        }
                    }
                }
            }
        }
        self.run(id).confirms.push(c);
    }

    /// An `intent` line: an attempt on a path.
    pub(crate) fn intent(&mut self, id: &str, v: &Value) {
        if let Some(Ok(bytes)) = line_path(v) {
            let r = self.run(id);
            r.intents += 1;
            r.paths.push(("intent", bytes));
        }
        // An unreadable path is reported by the generic check of the line itself.
    }

    /// A `result` that is not a skip: a delete, whole or partial.
    pub(crate) fn result(&mut self, id: &str, v: &Value) {
        if let Some(Ok(bytes)) = line_path(v) {
            let r = self.run(id);
            r.results += 1;
            r.paths.push(("result", bytes));
        }
    }

    /// A skipped `result` for a path whose `intent` has no result yet. The engine writes it when a delete that
    /// began is stopped by a mount point below the item (`skipped: cross-device`, the Partial of the plan): it
    /// ends that attempt, it is not a skip that never had one. A second result for a path that is already
    /// answered stays a skip, so it is still counted twice.
    pub(crate) fn follows_intent(&self, id: &str, v: &Value) -> bool {
        let Some(Ok(bytes)) = line_path(v) else {
            return false;
        };
        let Some(run) = self.runs.get(id) else {
            return false;
        };
        let of = |what: &str| {
            run.paths
                .iter()
                .filter(|(w, p)| *w == what && *p == bytes)
                .count()
        };
        of("intent") > of("result")
    }

    pub(crate) fn skipped(&mut self, id: &str, v: &Value) {
        let r = self.run(id);
        r.skips += 1;
        if let Some(Ok(b)) = line_path(v) {
            r.paths.push(("skip", b));
        }
    }

    /// Judges every explorer run at the end of the log and appends the violations.
    pub(crate) fn finish(&self, report: &mut VerifyReport) {
        for id in &self.order {
            let run = &self.runs[id];
            if !run.explorer && run.confirms.is_empty() {
                continue;
            }
            let mut bad = |path: String, reason: String| {
                report.violations.push(Violation {
                    run_id: id.clone(),
                    kind: KIND.to_string(),
                    path,
                    reason,
                });
            };
            judge_run(run, &mut bad);
        }
    }
}

fn judge_run(run: &Run, bad: &mut dyn FnMut(String, String)) {
    let touched = run.intents + run.results + run.skips > 0 || run.ended;
    match run.confirms.as_slice() {
        [] => {
            if touched {
                bad(
                    String::new(),
                    "explorer run without an explorer_confirm line".into(),
                );
            }
        }
        [c] => judge_counts(run, c, bad),
        more => {
            bad(
                String::new(),
                format!("explorer run with {} explorer_confirm lines", more.len()),
            );
        }
    }
}

fn judge_counts(run: &Run, c: &Confirm, bad: &mut dyn FnMut(String, String)) {
    if c.typed != c.count.to_string() {
        bad(
            String::new(),
            format!(
                "explorer_confirm typed {:?} for a count of {}",
                c.typed, c.count
            ),
        );
    }
    if c.items_ok && c.items.len() as u64 != c.count {
        bad(
            String::new(),
            format!(
                "explorer_confirm lists {} items for a count of {}",
                c.items.len(),
                c.count
            ),
        );
    }
    if c.items_ok {
        for (what, p) in &run.paths {
            if !c.items.contains(p) {
                bad(
                    show(p),
                    format!("{what} for a path that the explorer_confirm line does not list"),
                );
            }
        }
    }
    let seen = run.intents + run.skips;
    if run.refused.is_some() {
        if run.intents > 0 {
            bad(
                String::new(),
                format!("refused run with {} intent lines", run.intents),
            );
        }
    } else if run.ended && !run.interrupted {
        if seen != c.count {
            bad(
                String::new(),
                format!(
                    "finished run: {} intents + {} skips is not the count {}",
                    run.intents, run.skips, c.count
                ),
            );
        }
        if run.results != run.intents {
            bad(
                String::new(),
                format!(
                    "finished run: {} intents but {} results",
                    run.intents, run.results
                ),
            );
        }
    } else if seen > c.count {
        bad(
            String::new(),
            format!(
                "{} run: {} intents + {} skips is more than the count {}",
                if run.ended {
                    "interrupted"
                } else {
                    "unfinished"
                },
                run.intents,
                run.skips,
                c.count
            ),
        );
    }
}

/// The directories the verifier treats as the tool's own: the three XDG directories of this tool and, for
/// each, the spelling a canonical path gives it.
pub fn own_dirs(
    state: &std::path::Path,
    cache_base: &std::path::Path,
    config_base: &std::path::Path,
) -> Vec<PathBuf> {
    let mut v = vec![
        state.to_path_buf(),
        cache_base.join("dunnage"),
        config_base.join("dunnage"),
        // The pre-rename directories stay protected until the owner moves them.
        cache_base.join("disk-cleanup"),
        config_base.join("disk-cleanup"),
    ];
    if let Some(parent) = state.parent() {
        v.push(parent.join("disk-cleanup"));
    }
    let canon: Vec<PathBuf> = v
        .iter()
        .filter_map(|p| {
            p.parent()
                .and_then(|d| std::fs::canonicalize(d).ok())
                .zip(p.file_name())
        })
        .map(|(d, n)| d.join(n))
        .collect();
    v.extend(canon);
    v.sort();
    v.dedup();
    v
}

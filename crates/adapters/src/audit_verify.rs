//! `dunnage audit verify` (Story 1.2.4, Task 1.2.4d): re-validates every logged deletion. Read-only: no
//! lock, no audit write, no state write.
//!
//! The comparison is with the owner-reviewed table `ci/allowed-roots.txt` (embedded) plus the roots of the
//! owner's `config.toml`, never with the `roots` that the run itself recorded, because a bug or a widened
//! `--root` writes both the deletion and the roots that would excuse it. The one exception is Steam: its
//! library list comes from Steam's own files and no flag can change it (`--root` never moves Steam bounds), so
//! the libraries recorded by the run are the only source there is.

use std::collections::BTreeMap;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::audit_verify_explorer::{self as explorer, ExplorerBook};

/// The reviewed table, compiled into the binary.
pub const EMBEDDED_TABLE: &str = include_str!("../../../ci/allowed-roots.txt");

const RECORDED_ONLY: &str = "inside the roots recorded by that run but not in the reviewed table or your config; if intended, add the root to your config";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Rule {
    Exact(String),
    ToolOwned(String),
    UnderRoots(Vec<String>),
    UnderLibrary(Vec<String>),
}

/// The parsed `ci/allowed-roots.txt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedTable {
    default_roots: Vec<String>,
    rules: BTreeMap<String, Vec<Rule>>,
}

impl AllowedTable {
    pub fn parse(text: &str) -> Result<AllowedTable, String> {
        let mut t = AllowedTable {
            default_roots: Vec::new(),
            rules: BTreeMap::new(),
        };
        for (n, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let bad = || format!("allowed-roots line {}: cannot read '{raw}'", n + 1);
            let mut w = line.split_whitespace();
            let (kind, rule) = (w.next().ok_or_else(bad)?, w.next().ok_or_else(bad)?);
            let rest: Vec<&str> = w.collect();
            if kind == "roots" && rule == "default" {
                t.default_roots.extend(rest.iter().map(ToString::to_string));
                continue;
            }
            let value = rest.first().ok_or_else(bad)?.to_string();
            let rule = match rule {
                "exact" => Rule::Exact(value),
                "tool-owned" => Rule::ToolOwned(value),
                "under-roots" => Rule::UnderRoots(segments(&value)),
                "under-library" => Rule::UnderLibrary(segments(&value)),
                _ => return Err(bad()),
            };
            t.rules.entry(kind.to_string()).or_default().push(rule);
        }
        Ok(t)
    }

    pub fn embedded() -> AllowedTable {
        AllowedTable::parse(EMBEDDED_TABLE).unwrap_or_else(|e| unreachable!("embedded table: {e}"))
    }

    /// Every `exact` path of the table, `~` kept (the consistency test compares it with the cache table).
    pub fn exact_paths(&self, kind: &str) -> Vec<String> {
        self.of(kind)
            .iter()
            .filter_map(|r| match r {
                Rule::Exact(p) => Some(p.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn tool_names(&self, kind: &str) -> Vec<String> {
        self.of(kind)
            .iter()
            .filter_map(|r| match r {
                Rule::ToolOwned(n) => Some(n.clone()),
                _ => None,
            })
            .collect()
    }

    fn of(&self, kind: &str) -> &[Rule] {
        self.rules.get(kind).map_or(&[], Vec::as_slice)
    }
}

fn segments(s: &str) -> Vec<String> {
    s.split('/').map(ToString::to_string).collect()
}

fn seg_matches(pat: &str, comp: &[u8]) -> bool {
    match pat.strip_suffix('*') {
        Some(prefix) => comp.starts_with(prefix.as_bytes()) && comp.len() > prefix.len(),
        None => comp == pat.as_bytes(),
    }
}

/// A lexically clean absolute path as its components; `None` for anything else.
pub(crate) fn components(p: &[u8]) -> Option<Vec<&[u8]>> {
    let rest = p.strip_prefix(b"/")?;
    if rest.is_empty() {
        return Some(Vec::new());
    }
    let comps: Vec<&[u8]> = rest.split(|b| *b == b'/').collect();
    comps
        .iter()
        .all(|c| !c.is_empty() && *c != b"." && *c != b"..")
        .then_some(comps)
}

pub(crate) fn under<'a>(p: &'a [&'a [u8]], root: &[&[u8]]) -> Option<&'a [&'a [u8]]> {
    p.starts_with(root).then(|| &p[root.len()..])
}

pub(crate) fn expand(raw: &str, home: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    match raw.strip_prefix("~/") {
        Some(rest) => {
            let mut b = home.as_os_str().as_bytes().to_vec();
            b.push(b'/');
            b.extend_from_slice(rest.as_bytes());
            b
        }
        None => raw.as_bytes().to_vec(),
    }
}

/// One logged removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub run_id: String,
    pub kind: String,
    /// The path with non-UTF-8 bytes shown as `\xNN`.
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub deletions: usize,
    pub runs: usize,
    pub violations: Vec<Violation>,
    /// Runs with a `run_start` and no `run_end`: listed, never counted as violations.
    pub unfinished: Vec<String>,
    /// Lines this binary could not judge: not JSON (a torn last line included) or a schema it skips. Each
    /// names the line; any of them makes the exit code non-zero (Task 6.6.5g, review C13).
    pub warnings: Vec<String>,
    /// A last line with no newline that does not parse (a write cut short by a crash): named and printed,
    /// never a failure by itself (Task 6.6.3c).
    pub notes: Vec<String>,
}

impl VerifyReport {
    pub fn summary_line(&self) -> String {
        format!(
            "audit verify: {} deletions in {} runs, {} outside the allowed roots",
            self.deletions,
            self.runs,
            self.violations.len()
        )
    }

    /// 2 a deletion, partial deletion or intent outside the allowed roots; 1 only unjudged lines.
    pub fn exit_code(&self) -> i32 {
        if self.violations.is_empty() {
            i32::from(!self.warnings.is_empty())
        } else {
            2
        }
    }
}

#[derive(Debug)]
pub enum VerifyError {
    Io(io::Error),
    /// A line of a schema this binary does not read.
    Schema(u64),
}

impl VerifyError {
    pub fn reason(&self) -> String {
        match self {
            VerifyError::Io(e) => e.to_string(),
            VerifyError::Schema(n) => {
                format!("the log holds schema {n}, which this binary does not read")
            }
        }
    }
}

/// What the comparison needs besides the log.
pub struct VerifyInputs<'a> {
    pub table: &'a AllowedTable,
    pub home: &'a Path,
    /// `roots` of the owner's `config.toml`, read now.
    pub config_roots: Option<&'a [PathBuf]>,
}

/// What only the verifying machine knows, read at verify time and never from the log: the tool's own state
/// directories (rule M5 of the explorer's marks).
#[derive(Debug, Clone, Default)]
pub struct VerifyExtras {
    pub own_dirs: Vec<PathBuf>,
}

impl VerifyInputs<'_> {
    /// The roots a deletion is judged against: the reviewed table's defaults (`~` expanded) plus the owner's
    /// `config.toml`, never the roots the run recorded about itself.
    pub(crate) fn trusted_roots(&self) -> Vec<Vec<u8>> {
        let mut roots: Vec<Vec<u8>> = self
            .table
            .default_roots
            .iter()
            .map(|r| expand(r, self.home))
            .collect();
        if let Some(extra) = self.config_roots {
            use std::os::unix::ffi::OsStrExt;
            roots.extend(extra.iter().map(|r| r.as_os_str().as_bytes().to_vec()));
        }
        roots
    }
}

pub(crate) fn show(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for chunk in bytes.utf8_chunks() {
        s.push_str(chunk.valid());
        for b in chunk.invalid() {
            let _ = write!(s, "\\x{b:02x}");
        }
    }
    s
}

pub(crate) fn unhex(h: &str) -> Option<Vec<u8>> {
    if !h.len().is_multiple_of(2) {
        return None;
    }
    (0..h.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok())
        .collect()
}

/// The roots one run recorded, kept only for the recorded-only message and for Steam libraries.
#[derive(Default)]
struct Recorded {
    scan_roots: Vec<Vec<u8>>,
    steam_libraries: Vec<Vec<u8>>,
}

fn path_list(v: &Value, key: &str) -> Vec<Vec<u8>> {
    v.get("roots")
        .and_then(|r| r.get(key))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|s| s.as_bytes().to_vec())
                .collect()
        })
        .unwrap_or_default()
}

fn matches_under(pat: &[String], roots: &[Vec<u8>], p: &[&[u8]]) -> bool {
    roots.iter().any(|r| {
        let Some(root) = components(r) else {
            return false;
        };
        let Some(rel) = under(p, &root) else {
            return false;
        };
        rel.len() >= pat.len()
            && rel.iter().all(|c| *c != b".git")
            && rel[rel.len() - pat.len()..]
                .iter()
                .zip(pat)
                .all(|(c, s)| seg_matches(s, c))
    })
}

fn matches_library(pat: &[String], libraries: &[Vec<u8>], p: &[&[u8]]) -> bool {
    libraries.iter().any(|l| {
        components(l)
            .and_then(|lib| under(p, &lib))
            .is_some_and(|rel| {
                rel.len() == pat.len() && rel.iter().zip(pat).all(|(c, s)| seg_matches(s, c))
            })
    })
}

enum Verdict {
    Inside,
    Outside(String),
}

fn judge(kind: &str, path: &[u8], inp: &VerifyInputs, rec: &Recorded) -> Verdict {
    let Some(p) = components(path) else {
        return Verdict::Outside(
            "path is relative or not lexically clean (a '..', '.', or empty component)".into(),
        );
    };
    let rules = inp.table.of(kind);
    if rules.is_empty() {
        return Verdict::Outside(format!("no rule in the reviewed table for kind {kind}"));
    }
    let home_bytes = {
        use std::os::unix::ffi::OsStrExt;
        inp.home.as_os_str().as_bytes()
    };
    let home = components(home_bytes).unwrap_or_default();
    let mut roots: Vec<Vec<u8>> = inp
        .table
        .default_roots
        .iter()
        .map(|r| expand(r, inp.home))
        .collect();
    if let Some(extra) = inp.config_roots {
        use std::os::unix::ffi::OsStrExt;
        roots.extend(extra.iter().map(|r| r.as_os_str().as_bytes().to_vec()));
    }
    for rule in rules {
        let ok = match rule {
            Rule::Exact(e) => path == expand(e, inp.home).as_slice(),
            Rule::ToolOwned(name) => under(&p, &home).is_some_and(|rel| {
                rel.len() >= 2 && rel.last().is_some_and(|l| *l == name.as_bytes())
            }),
            Rule::UnderRoots(pat) => matches_under(pat, &roots, &p),
            Rule::UnderLibrary(pat) => matches_library(pat, &rec.steam_libraries, &p),
        };
        if ok {
            return Verdict::Inside;
        }
    }
    let recorded_only = rules.iter().any(|r| match r {
        Rule::UnderRoots(pat) => matches_under(pat, &rec.scan_roots, &p),
        _ => false,
    });
    Verdict::Outside(if recorded_only {
        RECORDED_ONLY.to_string()
    } else {
        "outside the allowed roots".to_string()
    })
}

/// Reads the whole log (streaming) and compares each `removed` result that has a path.
pub fn verify_audit(log: impl BufRead, inp: &VerifyInputs) -> Result<VerifyReport, VerifyError> {
    verify_audit_with(log, inp, &VerifyExtras::default())
}

/// [`verify_audit`] plus what only the verifying machine knows (the tool's own directories, for the explorer's
/// rule M5). Without `extras.own_dirs` the explorer's own-state check has nothing to compare with.
pub fn verify_audit_with(
    mut log: impl BufRead,
    inp: &VerifyInputs,
    extras: &VerifyExtras,
) -> Result<VerifyReport, VerifyError> {
    let mut report = VerifyReport::default();
    let mut recorded: BTreeMap<String, Recorded> = BTreeMap::new();
    let mut open: Vec<String> = Vec::new();
    let mut explorer = ExplorerBook::default();
    let mut raw = String::new();
    let mut n = 0usize;
    loop {
        raw.clear();
        if log.read_line(&mut raw).map_err(VerifyError::Io)? == 0 {
            break;
        }
        n += 1;
        let torn = !raw.ends_with('\n');
        let line = raw.trim_end_matches('\n');
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            if torn {
                report.notes.push(format!(
                    "line {n}: unfinished last line (no newline, not valid JSON): a write cut short; it was not checked"
                ));
            } else {
                report
                    .warnings
                    .push(format!("line {n}: not valid JSON, so it was not checked"));
            }
            continue;
        };
        if let Some(schema) = v.get("schema").and_then(Value::as_u64) {
            if schema == 2 {
                report
                    .warnings
                    .push(format!("line {n}: schema 2 is not checked by this binary"));
                continue;
            }
            if schema != 1 {
                return Err(VerifyError::Schema(schema));
            }
        }
        let run_id = v
            .get("run_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match v.get("type").and_then(Value::as_str) {
            Some("run_start") => {
                report.runs += 1;
                open.push(run_id.clone());
                explorer.run_start(&run_id, &v);
                recorded.insert(
                    run_id,
                    Recorded {
                        scan_roots: path_list(&v, "scan_roots"),
                        steam_libraries: path_list(&v, "steam_libraries"),
                    },
                );
            }
            Some("run_end") => {
                explorer.run_end(&run_id, &v);
                open.retain(|r| *r != run_id);
            }
            Some("explorer_confirm") => explorer.confirm(&run_id, &v, &mut report),
            Some("result") if v.get("status").and_then(Value::as_str) == Some("removed") => {
                explorer.result(&run_id, &v);
                check_result(&v, &run_id, inp, extras, recorded.get(&run_id), &mut report);
            }
            // A failed or interrupted delete may have removed entries below the path before it stopped.
            Some("result") if !is_skipped(&v) => {
                explorer.result(&run_id, &v);
                check_attempt(
                    &v,
                    "partial or failed delete",
                    &run_id,
                    inp,
                    extras,
                    recorded.get(&run_id),
                    &mut report,
                );
            }
            Some("result") if explorer.follows_intent(&run_id, &v) => {
                explorer.result(&run_id, &v);
                check_attempt(
                    &v,
                    "partial delete",
                    &run_id,
                    inp,
                    extras,
                    recorded.get(&run_id),
                    &mut report,
                );
            }
            Some("result") => explorer.skipped(&run_id, &v),
            Some("intent") => {
                explorer.intent(&run_id, &v);
                check_attempt(
                    &v,
                    "intent",
                    &run_id,
                    inp,
                    extras,
                    recorded.get(&run_id),
                    &mut report,
                );
            }
            _ => {}
        }
    }
    explorer.finish(&mut report);
    report.unfinished = open;
    Ok(report)
}

fn is_skipped(v: &Value) -> bool {
    v.get("status")
        .is_some_and(|s| s.get("skipped").is_some() || s.as_str() == Some("skipped"))
}

/// The path of a `result` (`path`) or an `intent` (`locator.path`).
fn attempt_path(v: &Value) -> Option<&str> {
    v.get("path").and_then(Value::as_str).or_else(|| {
        v.get("locator")
            .and_then(|l| l.get("path"))
            .and_then(Value::as_str)
    })
}

/// The exact bytes of the path of a `result` or an `intent` line: `path_hex` when the line has one (it must
/// be valid hex that agrees with the lossy `path`), else `path`. `Err` is the reason the line is a violation.
pub(crate) fn line_path(v: &Value) -> Option<Result<Vec<u8>, (String, &'static str)>> {
    let path = attempt_path(v)?;
    Some(match v.get("path_hex").and_then(Value::as_str) {
        None => Ok(path.as_bytes().to_vec()),
        Some(h) => match unhex(h) {
            None => Err((path.to_string(), "path_hex is not valid hex")),
            Some(b) if String::from_utf8_lossy(&b) != path => {
                Err((show(&b), "path and path_hex differ"))
            }
            Some(b) => Ok(b),
        },
    })
}

/// One violation judged by the table (or, for the explorer's kind, by its code rule).
fn judge_line(
    kind: &str,
    bytes: &[u8],
    inp: &VerifyInputs,
    extras: &VerifyExtras,
    rec: &Recorded,
) -> Verdict {
    if kind == explorer::KIND {
        return match explorer::judge(bytes, inp, extras) {
            None => Verdict::Inside,
            Some(reason) => Verdict::Outside(reason),
        };
    }
    judge(kind, bytes, inp, rec)
}

/// A `Partial` result or an `intent` whose path the table does not allow. It is not a counted deletion.
fn check_attempt(
    v: &Value,
    what: &str,
    run_id: &str,
    inp: &VerifyInputs,
    extras: &VerifyExtras,
    rec: Option<&Recorded>,
    report: &mut VerifyReport,
) {
    let kind = v.get("kind").and_then(Value::as_str).unwrap_or("");
    let violation = |path: String, reason: String| Violation {
        run_id: run_id.to_string(),
        kind: kind.to_string(),
        path,
        reason: format!("{what}: {reason}"),
    };
    let bytes = match line_path(v) {
        None => return,
        Some(Err((shown, why))) => {
            report.violations.push(violation(shown, why.to_string()));
            return;
        }
        Some(Ok(b)) => b,
    };
    let none = Recorded::default();
    if let Verdict::Outside(reason) = judge_line(kind, &bytes, inp, extras, rec.unwrap_or(&none)) {
        report.violations.push(violation(show(&bytes), reason));
    }
}

fn check_result(
    v: &Value,
    run_id: &str,
    inp: &VerifyInputs,
    extras: &VerifyExtras,
    rec: Option<&Recorded>,
    report: &mut VerifyReport,
) {
    let Some(path) = v.get("path").and_then(Value::as_str) else {
        return;
    };
    report.deletions += 1;
    let kind = v
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let violation = |path: String, reason: &str| Violation {
        run_id: run_id.to_string(),
        kind: kind.clone(),
        path,
        reason: reason.to_string(),
    };
    let bytes = match line_path(v) {
        Some(Ok(b)) => b,
        Some(Err((shown, why))) => {
            report.violations.push(violation(shown, why));
            return;
        }
        None => path.as_bytes().to_vec(),
    };
    let none = Recorded::default();
    if let Verdict::Outside(reason) = judge_line(&kind, &bytes, inp, extras, rec.unwrap_or(&none)) {
        report.violations.push(violation(show(&bytes), &reason));
    }
}

/// `verify_audit` over a file; an unreadable log is `VerifyError::Io`.
pub fn verify_file(path: &Path, inp: &VerifyInputs) -> Result<VerifyReport, VerifyError> {
    verify_file_with(path, inp, &VerifyExtras::default())
}

/// [`verify_file`] with the verifying machine's own directories.
pub fn verify_file_with(
    path: &Path,
    inp: &VerifyInputs,
    extras: &VerifyExtras,
) -> Result<VerifyReport, VerifyError> {
    let f = std::fs::File::open(path).map_err(VerifyError::Io)?;
    verify_audit_with(io::BufReader::new(f), inp, extras)
}

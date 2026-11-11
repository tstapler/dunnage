//! `RealAuditSink` and the reader of `audit.jsonl` (Story 2.2.1, Task 2.2.1a): one `write_all` per line on an
//! `O_APPEND` file, `sync_data` after the lines that must survive a crash, and the `audit.reserve` release when
//! the disk is full.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use disk_cleanup_core::audit_types::AUDIT_SCHEMA;
use disk_cleanup_core::ports::AuditSink;
use disk_cleanup_core::{
    AuditRecord, ExplorerConfirm, Intent, ItemResult, Locator, RunEnd, RunStart, Warning,
};
use serde::Serialize;

use crate::state::{chown_if, is_no_space, release_reserve, Owner};

const PRIVATE: u32 = 0o600;

/// Appends audit lines. The file is opened by `run_start` (so an unwritable log aborts before any delete) and
/// kept open for the run.
pub struct RealAuditSink {
    path: PathBuf,
    reserve: PathBuf,
    owner: Option<Owner>,
    file: Mutex<Option<File>>,
}

#[derive(Serialize)]
struct ReserveReleased<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    run_id: &'a str,
    ts: i64,
}

impl RealAuditSink {
    pub fn new(path: PathBuf, reserve: PathBuf, owner: Option<Owner>) -> RealAuditSink {
        RealAuditSink {
            path,
            reserve,
            owner,
            file: Mutex::new(None),
        }
    }

    fn open(&self) -> io::Result<File> {
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(PRIVATE)
            .open(&self.path)?;
        chown_if(self.owner, &self.path);
        Ok(f)
    }

    fn line<T: Serialize>(rec: &T) -> io::Result<Vec<u8>> {
        let mut v = serde_json::to_vec(rec).map_err(io::Error::other)?;
        v.push(b'\n');
        Ok(v)
    }

    /// One `write_all` of one whole line. On `ENOSPC` the reserve is truncated, a `reserve_released` line is
    /// appended, and the line is tried once more.
    fn append(&self, line: &[u8], run_id: &str, ts: i64, sync: bool) -> io::Result<()> {
        let mut guard = self
            .file
            .lock()
            .map_err(|_| io::Error::other("audit sink lock poisoned"))?;
        if guard.is_none() {
            *guard = Some(self.open()?);
        }
        let Some(f) = guard.as_mut() else {
            return Err(io::Error::other("audit file not open"));
        };
        // A failed `write_all` can leave a partial line; cut back to the length before it so the log never holds
        // a torn line (the run lock keeps other writers out while we do).
        let len_before = f.metadata()?.len();
        match f.write_all(line) {
            Ok(()) => {}
            Err(e) if is_no_space(&e) => {
                let _ = f.set_len(len_before);
                if !release_reserve(&self.reserve)? {
                    return Err(e);
                }
                let note = Self::line(&ReserveReleased {
                    kind: "reserve_released",
                    run_id,
                    ts,
                })?;
                let len_before = f.metadata()?.len();
                if let Err(e) = f.write_all(&note).and_then(|()| f.write_all(line)) {
                    let _ = f.set_len(len_before);
                    return Err(e);
                }
            }
            Err(e) => {
                let _ = f.set_len(len_before);
                return Err(e);
            }
        }
        if sync {
            f.sync_data()?;
        }
        Ok(())
    }
}

/// A path as the log holds it: serde refuses a non-UTF-8 `PathBuf`, so the line carries the lossy text and
/// `path_hex` (the exact bytes) only for such names.
fn lossless(p: &Path) -> (PathBuf, Option<String>) {
    use std::os::unix::ffi::OsStrExt;
    if p.to_str().is_some() {
        return (p.to_path_buf(), None);
    }
    let hex = p
        .as_os_str()
        .as_bytes()
        .iter()
        .fold(String::new(), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        });
    (PathBuf::from(p.to_string_lossy().into_owned()), Some(hex))
}

/// Every line carries `schema` so a reader can check each line, not only `run_start`.
fn with_hex(rec: &AuditRecord, hex: Option<String>) -> io::Result<Vec<u8>> {
    let mut v = serde_json::to_value(rec).map_err(io::Error::other)?;
    if let Some(o) = v.as_object_mut() {
        o.entry("schema").or_insert(AUDIT_SCHEMA.into());
        if let Some(h) = hex {
            o.insert("path_hex".into(), h.into());
        }
    }
    RealAuditSink::line(&v)
}

impl AuditSink for RealAuditSink {
    fn run_start(&self, rec: &RunStart) -> io::Result<()> {
        let line = with_hex(&AuditRecord::RunStart(rec.clone()), None)?;
        self.append(&line, &rec.run_id, rec.ts, true)
    }

    fn intent(&self, rec: &Intent) -> io::Result<()> {
        let mut r = rec.clone();
        let hex = match &mut r.locator {
            Locator::Path(p) | Locator::Steam(_, p) => {
                let (lossy, hex) = lossless(p);
                *p = lossy;
                hex
            }
            Locator::Docker(_) => None,
        };
        let line = with_hex(&AuditRecord::Intent(r), hex)?;
        self.append(&line, &rec.run_id, rec.ts, false)
    }

    fn result(&self, rec: &ItemResult) -> io::Result<()> {
        let mut r = rec.clone();
        let hex = r.0.path.as_mut().and_then(|p| {
            let (lossy, hex) = lossless(p);
            *p = lossy;
            hex
        });
        let line = with_hex(&AuditRecord::Result(r), hex)?;
        self.append(&line, &rec.0.run_id, rec.0.ts, true)
    }

    fn run_end(&self, rec: &RunEnd) -> io::Result<()> {
        let line = with_hex(&AuditRecord::RunEnd(rec.clone()), None)?;
        self.append(&line, &rec.run_id, rec.ts, true)
    }

    /// Synced like `run_start`: the paths it names are what `audit verify` judges when the unsynced `intent`
    /// lines of a crashed run are lost.
    fn explorer_confirm(&self, rec: &ExplorerConfirm) -> io::Result<()> {
        let mut r = rec.clone();
        for item in &mut r.items {
            let (lossy, hex) = lossless(&item.path);
            item.path = lossy;
            item.path_hex = hex;
        }
        let line = with_hex(&AuditRecord::ExplorerConfirm(r), None)?;
        self.append(&line, &rec.run_id, rec.ts, true)
    }
}

// ---------------------------------------------------------------------------------------------------------
// reading

/// The log carries a schema this binary does not read (`design/ux.md` 3.5, `AuditSchema`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditSchemaError {
    pub path: PathBuf,
    pub found: u64,
}

impl AuditSchemaError {
    pub fn text(&self) -> String {
        let p = crate::state::shell_quote_path(&self.path);
        let old =
            crate::state::shell_quote_path(&PathBuf::from(format!("{}.old", self.path.display())));
        format!(
            "error: the audit log {} has an unknown schema version ({}). Nothing was deleted. Move it aside ('mv -- {p} {old}') and run again; 'disk-cleanup audit verify --log {old}' reads it if it can.",
            self.path.display(),
            self.found,
        )
    }

    pub fn exit_code(&self) -> i32 {
        2
    }
}

impl std::fmt::Display for AuditSchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text())
    }
}

impl std::error::Error for AuditSchemaError {}

/// The last run of the log that has a `run_start` but no `run_end`, if any. Unknown fields are ignored, a line
/// with `schema: 2` is skipped, any other schema is an error, and a torn last line is skipped. Streams the file.
pub fn unfinished_run(path: &Path) -> Result<Option<String>, AuditSchemaError> {
    let Ok(f) = File::open(path) else {
        return Ok(None);
    };
    let mut open: Option<String> = None;
    for line in BufReader::new(f).lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(schema) = v.get("schema").and_then(serde_json::Value::as_u64) {
            if schema == 2 {
                continue;
            }
            if schema != u64::from(AUDIT_SCHEMA) {
                return Err(AuditSchemaError {
                    path: path.to_path_buf(),
                    found: schema,
                });
            }
        }
        let run_id = v.get("run_id").and_then(|r| r.as_str());
        match (v.get("type").and_then(|t| t.as_str()), run_id) {
            (Some("run_start"), Some(id)) => open = Some(id.to_string()),
            (Some("run_end"), Some(id)) if open.as_deref() == Some(id) => open = None,
            _ => {}
        }
    }
    Ok(open)
}

/// The warning to print when the previous run did not finish.
pub fn unclean_run_warning(path: &Path) -> Result<Option<Warning>, AuditSchemaError> {
    Ok(
        unfinished_run(path)?.map(|run_id| Warning::PreviousRunUnfinished {
            run_id,
            audit_path: path.to_path_buf(),
        }),
    )
}

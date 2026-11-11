//! `disk-cleanup status [PATH] [--fs DIR] [--json]` (Story 1.2.4, Task 1.2.4e, `design/ux.md` S4). Takes no
//! lock and writes neither the audit log nor `last_plan`; the caller appends the one outcome line.

use std::path::Path;

use disk_cleanup_adapters::status::{assess, read_status};
use disk_cleanup_core::ports::SpaceMeter;
use disk_cleanup_core::{DiskVerdict, FatalError};

use crate::args::StatusArgs;
use crate::render::render_status;
use crate::render_json::{fatal_entry, status_doc_with, to_text, StatusExtra};
use crate::terminal::Terminal;
use crate::view::FsBlock;

/// What a `status` run printed and what the outcome record needs.
#[derive(Debug)]
pub struct StatusRun {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    /// `(verdict, free bytes, unallocated bytes)` when the state was read.
    pub outcome: Option<(DiskVerdict, u64, Option<u64>)>,
}

fn usage(stderr: String) -> StatusRun {
    StatusRun {
        code: 64,
        stdout: String::new(),
        stderr,
        outcome: None,
    }
}

pub fn run(
    args: &StatusArgs,
    json: bool,
    meter: &dyn SpaceMeter,
    term: &Terminal,
    home: Option<&Path>,
) -> StatusRun {
    if std::fs::symlink_metadata(&args.path).is_err() {
        return usage(format!(
            "error: path {} does not exist or cannot be read. Nothing was changed. Pass an existing path, or none for '/'.\n",
            args.path.display()
        ));
    }
    let read = match read_status(meter, &args.path, args.fs.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            let fatal = FatalError::StateUnreadable {
                path: e.path,
                reason: e.reason,
            };
            let doc = json.then(|| {
                to_text(&status_doc_with(
                    &args.path,
                    None,
                    &[fatal_entry(&fatal)],
                    fatal.exit_code(),
                    &StatusExtra::default(),
                ))
            });
            return StatusRun {
                code: fatal.exit_code(),
                stdout: doc.unwrap_or_default(),
                stderr: format!("{}\n", fatal.text()),
                outcome: None,
            };
        }
    };
    let (verdict, findings) = assess(&read.snapshot);
    let code = match verdict {
        DiskVerdict::Warn => 1,
        DiskVerdict::Crit => 2,
        DiskVerdict::Ok | DiskVerdict::Unknown => 0,
    };
    let outcome = Some((
        verdict,
        read.snapshot.avail_bytes,
        read.snapshot.btrfs.map(|b| b.unallocated),
    ));
    let fs = FsBlock {
        snapshot: read.snapshot,
        verdict,
    };
    let stdout = if json {
        let extra = StatusExtra {
            fs_dir: read.fs_dir,
            allocation: read.allocation,
            findings: findings
                .into_iter()
                .map(|f| (f.severity.to_string(), f.message))
                .collect(),
        };
        to_text(&status_doc_with(&args.path, Some(&fs), &[], code, &extra))
    } else {
        render_status(&fs, home, term)
    };
    StatusRun {
        code,
        stdout,
        stderr: String::new(),
        outcome,
    }
}

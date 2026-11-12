//! The headroom line (`tui/ux-explorer.md` 3.3, UX-T15): free space of one filesystem on one line, in the
//! wide, compact or narrow form that fits, ending with the verdict word of
//! `dunnage_scan::health::assess` (the metascope assessment).

use std::path::Path;

use dunnage_scan::health::{assess, Thresholds, Verdict};

use super::fmt::fmt_size_cell;
use super::layout::{cells, cut_name};
use crate::escape::{escape, escape_os};
use crate::snapshot_types::FsSnapshot;

fn verdict_word(v: Verdict) -> &'static str {
    match v {
        Verdict::Ok => "OK",
        Verdict::Warn => "WARN",
        Verdict::Crit => "CRIT",
    }
}

/// Percent of `total` that `used` is, round half up; `n/a` for an empty filesystem.
fn used_percent(used: u64, total: u64) -> String {
    if total == 0 {
        return "n/a".into();
    }
    let (u, t) = (u128::from(used), u128::from(total));
    format!("{}%", (200 * u + t) / (2 * t))
}

/// The line for `snapshot` (or the reason it could not be read) of the filesystem holding `path`, at most
/// `w` cells wide. The forms, widest that fits first:
///
/// - btrfs: `/ btrfs  avail A  unallocated U  data slack D  meta slack M  WARN`, then
///   `/ btrfs  avail A  unalloc U  slack data D meta M  WARN`, then `/ avail A  unalloc U  WARN`;
/// - other filesystems: `/ ext4  avail A of T  used 96%  OK`;
/// - unreadable: `/ filesystem state unreadable (reason)  unknown`.
///
/// A line that fits in no form is cut with `...`.
pub fn headroom_line(path: &Path, snapshot: Result<&FsSnapshot, &str>, w: usize) -> String {
    let p = escape_os(path.as_os_str(), true);
    let forms = forms_for(&p, snapshot);
    if let Some(f) = forms.iter().find(|f| cells(f) <= w) {
        return f.clone();
    }
    // No form fits: shorten the path label so the verdict word stays.
    let last = &forms[forms.len() - 1];
    let room = w.saturating_sub(cells(last) - cells(&p));
    let line = forms_for(&cut_name(&p, room), snapshot)
        .pop()
        .unwrap_or_default();
    if cells(&line) <= w {
        line
    } else {
        cut_name(&line, w)
    }
}

fn forms_for(label: &str, snapshot: Result<&FsSnapshot, &str>) -> Vec<String> {
    match snapshot {
        Err(reason) => vec![format!(
            "{label} filesystem state unreadable ({})  unknown",
            escape(reason.as_bytes(), true)
        )],
        Ok(snap) => match &snap.btrfs {
            None => vec![format!(
                "{label} {}  avail {} of {}  used {}  OK",
                escape(snap.fs_type.as_bytes(), true),
                fmt_size_cell(snap.avail_bytes),
                fmt_size_cell(snap.total_bytes),
                used_percent(snap.used_bytes, snap.total_bytes)
            )],
            Some(alloc) => {
                let verdict = verdict_word(
                    assess(
                        alloc.unallocated,
                        alloc.metadata_headroom_bytes(),
                        &Thresholds::default(),
                    )
                    .verdict,
                );
                let (avail, unalloc) = (
                    fmt_size_cell(snap.avail_bytes),
                    fmt_size_cell(alloc.unallocated),
                );
                let (data, meta) = (
                    fmt_size_cell(alloc.data.headroom_bytes()),
                    fmt_size_cell(alloc.metadata_headroom_bytes()),
                );
                // The compact form names the unit once when both slacks share it: `data 51.2 meta 1.9 GiB`.
                let slack = match (data.rsplit_once(' '), meta.rsplit_once(' ')) {
                    (Some((d, du)), Some((m, mu))) if du == mu => format!("data {d} meta {m} {mu}"),
                    _ => format!("data {data} meta {meta}"),
                };
                vec![
                    format!("{label} btrfs  avail {avail}  unallocated {unalloc}  data slack {data}  meta slack {meta}  {verdict}"),
                    format!("{label} btrfs  avail {avail}  unalloc {unalloc}  slack {slack}  {verdict}"),
                    format!("{label} avail {avail}  unalloc {unalloc}  {verdict}"),
                ]
            }
        },
    }
}

//! `dunnage scan`, the read-only scanner (Story 1.2.5, ADR-007): the former `metascope scan`, ported
//! from `crates/cli/src/main.rs` of metascope at 3d36581 (lines 234-388).
//!
//! It is dispatched before any `Env`, lock, audit sink or `RunGuard` exists. Its only write is the walker
//! cache file, `scan.cache`, kept apart from the cleaner's own cache so the cleaner's speed protocol is
//! unaffected. Exit codes are the cleaner's: 0, 2 fatal, 64 usage, 130/143 on a signal.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use dunnage_scan::scan::{self, BackendChoice, CategoryStats, ScanOptions, ScanReport};
use dunnage_scan::walker::CacheConfig;

use crate::app::Io;
use crate::args::{ScanArgs, ScanBackend, ScanBy, ScanSort};

const EXIT_FATAL: i32 = 2;
const EXIT_USAGE: i32 = 64;

/// The process entry used by `main`: signal handlers (a signal exits 128 + n at once, nothing is mutated that
/// a kill can break), then the run over the real stdout and stderr.
pub fn run_wired(args: &ScanArgs, json: bool) -> i32 {
    if let Err(e) = crate::wiring::run_guard::install_plan() {
        eprintln!("error: cannot install signal handlers ({e}).");
        return EXIT_FATAL;
    }
    // Unlocked handles: the signal watcher writes its line to stderr, and a lock held by this thread while
    // it blocks (a cache file on a FIFO, a stalled mount) would deadlock the exit.
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    run_process(
        args,
        json,
        &mut Io {
            out: &mut out,
            err: &mut err,
        },
    )
}

/// Runs against the process environment (`HOME`, `XDG_CACHE_HOME`).
pub fn run_process(args: &ScanArgs, json: bool, io: &mut Io) -> i32 {
    run(args, json, &|k| std::env::var_os(k), io)
}

pub fn run(
    args: &ScanArgs,
    json: bool,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    io: &mut Io,
) -> i32 {
    let roots = if args.paths.is_empty() {
        default_roots(env)
    } else {
        args.paths.clone()
    };
    for root in &roots {
        if !root.is_dir() {
            let _ = writeln!(
                io.err,
                "error: '{}' is not a directory that exists. Nothing was scanned.",
                root.display()
            );
            return EXIT_USAGE;
        }
    }
    let opts = options(args, roots, env);
    let mut report = match scan::scan(&opts) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(io.err, "error: {e}");
            return EXIT_FATAL;
        }
    };
    sort_report(&mut report, args.sort);
    let text = if json {
        match serde_json::to_string_pretty(&report) {
            Ok(t) => t + "\n",
            Err(e) => {
                let _ = writeln!(io.err, "error: {e}");
                return EXIT_FATAL;
            }
        }
    } else {
        table(&report, args.top, args.by)
    };
    let _ = io.out.write_all(text.as_bytes());
    0
}

fn options(
    args: &ScanArgs,
    roots: Vec<PathBuf>,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> ScanOptions {
    let mut opts = ScanOptions::new(roots);
    opts.backend = match args.backend {
        ScanBackend::Auto => BackendChoice::Auto,
        ScanBackend::Walker => BackendChoice::Walker,
        ScanBackend::Btrfs => BackendChoice::BtrfsTree,
    };
    opts.cross_fs = args.cross_fs;
    opts.max_duration = (args.max_seconds > 0).then(|| Duration::from_secs(args.max_seconds));
    opts.threads = args.threads;
    opts.cache = (!args.no_cache).then(|| CacheConfig {
        path: args
            .cache_file
            .clone()
            .unwrap_or_else(|| default_cache_file(env)),
        rescan: args.rescan,
        max_age: Duration::from_secs(args.cache_max_age_hours.saturating_mul(3600)),
    });
    opts
}

/// `$XDG_CACHE_HOME/dunnage/scan.cache`, else `$HOME/.cache/...`.
pub fn default_cache_file(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> PathBuf {
    env("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dunnage/scan.cache")
}

fn default_roots(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Vec<PathBuf> {
    let Some(home) = env("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    ["Programming", "code", "dotfiles"]
        .iter()
        .map(|d| home.join(d))
        .filter(|p| p.is_dir())
        .collect()
}

fn sort_key(sort: ScanSort, stats: &CategoryStats, inodes: u64, meta: u64) -> u64 {
    match sort {
        ScanSort::Inodes => inodes,
        ScanSort::Meta => meta,
        ScanSort::Size => stats.combined().size_bytes,
    }
}

/// Descending by the key, stable: ties keep the scanner's order, as the old tool did.
pub fn sort_report(report: &mut ScanReport, sort: ScanSort) {
    report.families.sort_by_key(|f| {
        std::cmp::Reverse(sort_key(sort, &f.stats, f.inodes, f.metadata_est_bytes))
    });
    report.groups.sort_by_key(|g| {
        std::cmp::Reverse(sort_key(sort, &g.stats, g.inodes, g.metadata_est_bytes))
    });
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    #[allow(clippy::cast_precision_loss)]
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// The report table, byte for byte the old `print_scan`.
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
pub fn table(report: &ScanReport, top: usize, by: ScanBy) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "{:>10} {:>10} {:>12} {:>12} {:>9} {:>10} {:>4}  PATH",
        "INODES", "SOURCE", "NODE_MODULES", "CARGO_TARGET", "GIT", "META~", "N"
    );
    let rows: Vec<(&CategoryStats, u64, u64, usize, &PathBuf)> = match by {
        ScanBy::Repo => report
            .families
            .iter()
            .map(|f| {
                (
                    &f.stats,
                    f.inodes,
                    f.metadata_est_bytes,
                    f.checkouts,
                    &f.repo,
                )
            })
            .collect(),
        ScanBy::Checkout => report
            .groups
            .iter()
            .map(|g| (&g.stats, g.inodes, g.metadata_est_bytes, 1, &g.path))
            .collect(),
    };
    for (stats, inodes, meta, n, path) in rows.into_iter().take(top) {
        let _ = writeln!(
            o,
            "{:>10} {:>10} {:>12} {:>12} {:>9} {:>10} {:>4}  {}",
            inodes,
            stats.source.inodes,
            stats.node_modules.inodes,
            stats.cargo_target.inodes,
            stats.git.inodes,
            human(meta),
            n,
            path.display()
        );
    }

    let t = &report.totals;
    let all = t.combined().inodes.max(1);
    let _ = write!(o, "\nBY CATEGORY\n");
    for (name, s) in [
        ("source", &t.source),
        ("node_modules", &t.node_modules),
        ("cargo target", &t.cargo_target),
        (".git", &t.git),
    ] {
        let _ = writeln!(
            o,
            "  {name:<13} {:>10} inodes ({:>4.1}%)  {:>9} dirs  {:>10}  meta~ {}",
            s.inodes,
            s.inodes as f64 * 100.0 / all as f64,
            s.dirs,
            human(s.size_bytes),
            human(s.metadata_cost().total())
        );
    }
    let _ = writeln!(
        o,
        "  total         {:>10} inodes  meta~ {} logical (x2 on DUP)",
        t.combined().inodes,
        human(t.metadata_est_bytes())
    );

    if !report.duplicates.is_empty() {
        let _ = write!(
            o,
            "\nDUPLICATE BUILD DIRS (same lockfile content; heuristic)\n"
        );
        for d in report.duplicates.iter().take(top) {
            let _ = writeln!(
                o,
                "  {:?} x{}  {}..{} inodes each  redundant {} inodes, meta~ {}  e.g. {}",
                d.kind,
                d.members.len(),
                d.min_inodes,
                d.max_inodes,
                d.redundant_inodes,
                human(d.redundant_metadata_est_bytes),
                d.members[0].display()
            );
        }
    }

    let _ = write!(
        o,
        "\nbackend {}: {} dirs listed, {} from cache\n",
        report.backend, report.dirs_listed, report.dirs_cached
    );
    for n in &report.notes {
        let _ = writeln!(o, "note: {n}");
    }
    let _ = writeln!(
        o,
        "{} build dirs, {} checkouts/groups; walk {} ms, total {} ms, {} unreadable{}",
        report.build_roots.len(),
        report.groups.len(),
        report.walk_ms,
        report.total_ms,
        report.errors,
        if report.truncated {
            ", TRUNCATED by --max-seconds (partial)"
        } else {
            ""
        }
    );
    let _ = writeln!(
        o,
        "~ = estimate ({}; xattrs and leaf slack not modeled)",
        if report.extents_exact {
            "extent counts are exact from the btrfs tree"
        } else {
            "extent counts are a lower bound"
        }
    );
    o
}

#[cfg(test)]
mod tests {
    use super::human;

    #[test]
    fn human_formats_units() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(1 << 30), "1.0 GiB");
    }
}

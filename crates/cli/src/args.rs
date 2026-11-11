//! The clap surface of `disk-cleanup` (Story 1.2.4, Task 1.2.4a, `design/ux.md` S8) and the one place that
//! turns a parse error into an exit code: help and version exit 0, every other clap error exits 64 (the
//! default of 2 collides with "fatal").
//!
//! There is no `--force`, `-y`, `--yes` or prompt (UX-4); `-n`/`--dry-run` are accepted no-ops that conflict
//! with `--apply` (UX-51).

use std::path::PathBuf;

use clap::error::{ContextKind, ErrorKind};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use disk_cleanup_core::plan::PlanIdArg;
use disk_cleanup_core::selector::{Discard, DockerVolumes, KindSelector, Selection, SelectorError};
use disk_cleanup_core::SelectorId;

use crate::terminal::ColorChoice;

pub const FORCE_TIP: &str = "Tip: there is no force or yes flag; use --kinds.";
pub const DRY_RUN_WITH_APPLY: &str = "error: -n/--dry-run and --apply contradict each other; drop one. Dry run is the default, so plain 'disk-cleanup' is the dry run.";

const ABOUT: &str = "Free disk space. Dry run by default: nothing is deleted without --apply.";

const AFTER_HELP: &str = "\
Exit codes: 0 ok, 1 partial failure (status: warn), 2 fatal (status: crit; audit verify: violations),
            3 btrfs starved, 64 usage, 74 status/audit verify could not read state,
            130 interrupted (SIGINT), 143 (SIGTERM)
Example: disk-cleanup --json | jq -r '.items[] | [.status, .kind, .est_bytes, .path] | @tsv'

Environment:
  DISK_CLEANUP_UNGATED=<reason>  audited break-glass for the gate-stamp check of an unconfined --apply;
                                 you must also type BYPASS on the terminal
  NO_COLOR                       turn colour off (--color always overrides it)";

// A command line is a set of independent switches.
#[allow(clippy::struct_excessive_bools)]
#[derive(Parser, Debug)]
#[command(
    name = "disk-cleanup",
    about = ABOUT,
    after_help = AFTER_HELP,
    disable_version_flag = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Delete. Without it nothing is deleted
    #[arg(long, conflicts_with = "dry_run")]
    pub apply: bool,

    /// Accepted for old habits: dry run is the default. Conflicts with --apply (exit 64)
    #[arg(short = 'n', long = "dry-run", conflicts_with = "apply")]
    pub dry_run: bool,

    /// Required with --apply when build-dirs, docker-volumes or steam is selected or --worktree-ignored
    /// discard is passed: the plan-id of a dry run of the same selection, made in the last 24h
    #[arg(long, value_name = "ID")]
    pub plan_id: Option<PlanIdArg>,

    /// Kinds to run, e.g. caches,worktrees (replaces the default)
    #[arg(long, value_name = "LIST")]
    pub kinds: Option<String>,

    /// Remove kinds from the selection
    #[arg(long, value_name = "LIST")]
    pub except: Option<String>,

    /// Scan root for build dirs and worktrees (repeatable; replaces the default roots)
    #[arg(long, value_name = "PATH")]
    pub root: Vec<PathBuf>,

    /// Age floor, e.g. 14d, 36h (build dirs never below 60m)
    #[arg(long, value_name = "AGE", value_parser = parse_age)]
    pub min_age: Option<u64>,

    /// Deprecated alias for --min-age <N>d
    #[arg(long, value_name = "DAYS", hide = true, conflicts_with = "min_age")]
    pub stale_days: Option<u64>,

    /// Size floor, e.g. 500MiB, 2GiB
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub min_size: Option<u64>,

    /// Keep this many newest duplicates of a build dir (needs build-dirs)
    #[arg(long, value_name = "N")]
    pub keep_newest: Option<u32>,

    /// Worker threads for deletes
    #[arg(long, value_name = "N", default_value_t = 4)]
    pub jobs: usize,

    /// Allow removing that class of unreferenced volume (needs docker-volumes)
    #[arg(long, value_enum, value_name = "anonymous|named")]
    pub docker_volumes: Option<VolumeClass>,

    /// Allow this exact volume past the database/size heuristics (repeatable; needs docker-volumes)
    #[arg(long, value_name = "NAME")]
    pub volume: Vec<String>,

    /// Allow prefixes that hold save-game files (needs steam)
    #[arg(long, value_enum, value_name = "discard")]
    pub steam_saves: Option<DiscardArg>,

    /// Allow worktrees that hold ignored files such as .env and *.sqlite (needs worktrees)
    #[arg(long, value_enum, value_name = "discard")]
    pub worktree_ignored: Option<DiscardArg>,

    /// Restrict to this absolute canonical path, an item of the selected kinds (repeatable)
    #[arg(long, value_name = "PATH")]
    pub only: Vec<PathBuf>,

    /// Rows printed (0 prints every row)
    #[arg(long, value_name = "N", default_value_t = 10)]
    pub top: usize,

    /// Row order
    #[arg(long, value_enum, value_name = "size|age|kind", default_value = "size")]
    pub sort: SortArg,

    /// Stop scanning after this many seconds (sizes become lower bounds)
    #[arg(long, value_name = "N")]
    pub max_seconds: Option<u64>,

    /// Ignore the walker cache and scan everything again
    #[arg(long)]
    pub rescan: bool,

    /// No progress lines on stderr
    #[arg(long)]
    pub no_progress: bool,

    /// Colour on stdout
    #[arg(
        long,
        value_enum,
        value_name = "auto|always|never",
        default_value = "auto",
        global = true
    )]
    pub color: ColorArg,

    /// Hidden alias for --color never
    #[arg(long, hide = true, global = true)]
    pub no_color: bool,

    /// One JSON document on stdout, nothing else
    #[arg(long, global = true)]
    pub json: bool,

    /// Print why each candidate was kept and every external command
    #[arg(short, long)]
    pub verbose: bool,

    /// Summary lines only
    #[arg(short, long, conflicts_with = "verbose")]
    pub quiet: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Filesystem usage and verdict (exit 0 ok, 1 warn, 2 crit)
    Status(StatusArgs),
    /// Read-only inode and metadata report per repo or checkout (the former `metascope scan`)
    #[command(after_help = SCAN_AFTER_HELP)]
    Scan(ScanArgs),
    /// List cleanable kinds, tiers and availability
    Kinds,
    /// Steam tools (steam list)
    Steam {
        #[command(subcommand)]
        command: SteamCommand,
    },
    /// Check the audit log: audit verify [--log PATH] [--json] (exit 0 clean, 2 a deletion outside the
    /// allowed roots, 74 log unreadable)
    Audit {
        #[command(subcommand)]
        command: AuditCommand,
    },
}

const SCAN_AFTER_HELP: &str = "\
The first run is cold: the old metascope/walker.cache is not migrated; this command keeps its own scan.cache.
Exit codes: 0 ok, 2 fatal, 64 usage (a nonexistent root), 130 interrupted (SIGINT), 143 (SIGTERM).
--json prints the scanner's own report document; it is not the schema_version 1 agent contract.
The old `metascope status` is the `disk-cleanup status` command.";

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanBy {
    Repo,
    Checkout,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanSort {
    Inodes,
    Meta,
    Size,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanBackend {
    Auto,
    Walker,
    Btrfs,
}

// Mirrors the flags of the old `metascope scan` one for one.
#[allow(clippy::struct_excessive_bools)]
#[derive(Args, Debug)]
pub struct ScanArgs {
    /// Roots to scan (default: ~/Programming, ~/code and ~/dotfiles, those that exist)
    pub paths: Vec<PathBuf>,
    /// Rows to print per table
    #[arg(long, default_value_t = 20)]
    pub top: usize,
    /// Table granularity: one row per repo family, or per checkout/worktree
    #[arg(long, value_enum, default_value = "repo")]
    pub by: ScanBy,
    /// Row order
    #[arg(long, value_enum, default_value = "inodes")]
    pub sort: ScanSort,
    /// Data source: walker (no root, uses the cache), btrfs (tree search, needs root), or auto
    #[arg(long, value_enum, default_value = "auto")]
    pub backend: ScanBackend,
    /// Do not read or write the incremental cache (walker only)
    #[arg(long)]
    pub no_cache: bool,
    /// Ignore cached directory records but write fresh ones (walker only)
    #[arg(long)]
    pub rescan: bool,
    /// Cache file (default: $XDG_CACHE_HOME/disk-cleanup/scan.cache)
    #[arg(long, value_name = "FILE")]
    pub cache_file: Option<PathBuf>,
    /// Re-list cached directories older than this many hours
    #[arg(long, default_value_t = 24)]
    pub cache_max_age_hours: u64,
    /// Accepted for compatibility; everything is counted
    #[arg(long, hide = true)]
    pub include_ignored: bool,
    /// Descend into other mounts and btrfs subvolumes (walker only)
    #[arg(long)]
    pub cross_fs: bool,
    /// Walker threads (0 = 3x CPUs, capped at 64)
    #[arg(long, default_value_t = 0)]
    pub threads: usize,
    /// Stop walking after this many seconds and report partial results (0 = no cap)
    #[arg(long, default_value_t = 120)]
    pub max_seconds: u64,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Path whose filesystem to inspect
    #[arg(default_value = "/")]
    pub path: PathBuf,
    /// Use this /sys/fs/btrfs/<uuid> directory instead of auto-detecting
    #[arg(long, value_name = "DIR")]
    pub fs: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum SteamCommand {
    /// Rank installed games by size x idle time / hours played (read-only)
    List {
        /// Row order
        #[arg(long, value_enum, default_value = "score")]
        sort: SteamSortArg,
        /// Only show games of at least this many GiB
        #[arg(long, value_name = "GIB")]
        min_gib: Option<f64>,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteamSortArg {
    Score,
    Size,
    Date,
    Playtime,
    Name,
}

#[derive(Subcommand, Debug)]
pub enum AuditCommand {
    /// Re-validate every logged deletion against the reviewed allowed roots
    Verify {
        /// The log to read (default: the audit log in the state directory)
        #[arg(long, value_name = "PATH")]
        log: Option<PathBuf>,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeClass {
    Anonymous,
    Named,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardArg {
    Discard,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortArg {
    Size,
    Age,
    Kind,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorArg {
    Auto,
    Always,
    Never,
}

/// `14d`, `36h`, `60m`, `90s` (a bare number is seconds), in seconds.
pub fn parse_age(s: &str) -> Result<u64, String> {
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: u64 = n
        .parse()
        .map_err(|_| format!("'{s}' is not an age (examples: 14d, 36h, 60m)"))?;
    let mult = match unit {
        "d" => 86_400,
        "h" => 3_600,
        "m" => 60,
        "s" | "" => 1,
        _ => return Err(format!("'{s}' is not an age (examples: 14d, 36h, 60m)")),
    };
    n.checked_mul(mult)
        .ok_or_else(|| format!("'{s}' is too large"))
}

/// `500MiB`, `2GiB`, `1G`, `4096` (a bare number is bytes), in bytes. Binary units only, as `format_size`.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: u64 = n
        .parse()
        .map_err(|_| format!("'{s}' is not a size (examples: 500MiB, 2GiB)"))?;
    let shift = match unit {
        "" | "B" => 0,
        "K" | "KiB" => 10,
        "M" | "MiB" => 20,
        "G" | "GiB" => 30,
        "T" | "TiB" => 40,
        _ => return Err(format!("'{s}' is not a size (examples: 500MiB, 2GiB)")),
    };
    n.checked_mul(1 << shift)
        .ok_or_else(|| format!("'{s}' is too large"))
}

/// What a parse produced: either the process is done (help, version, a usage error) or a command to run.
#[derive(Debug)]
pub enum Parsed {
    Exit {
        code: i32,
        stdout: String,
        stderr: String,
    },
    Run(Box<Cli>),
}

fn mentions_dry_run_and_apply(e: &clap::Error) -> bool {
    let named = |k: ContextKind| e.get(k).map(ToString::to_string).unwrap_or_default();
    let both = format!(
        "{} {}",
        named(ContextKind::InvalidArg),
        named(ContextKind::PriorArg)
    );
    both.contains("--apply") && (both.contains("--dry-run") || both.contains("-n"))
}

fn forced(args: &[String]) -> bool {
    args.iter()
        .any(|a| matches!(a.as_str(), "--force" | "-y" | "--yes"))
}

/// Parses `args` (without the program name). Never prints: the caller writes `stdout` and `stderr`.
pub fn parse<I, T>(args: I) -> Parsed
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let raw: Vec<std::ffi::OsString> = std::iter::once("disk-cleanup".into())
        .chain(args.into_iter().map(Into::into))
        .collect();
    let words: Vec<String> = raw
        .iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    match Cli::try_parse_from(&raw) {
        Ok(cli) => Parsed::Run(Box::new(cli)),
        Err(e) if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
            Parsed::Exit {
                code: 0,
                stdout: e.render().to_string(),
                stderr: String::new(),
            }
        }
        Err(e) if e.kind() == ErrorKind::ArgumentConflict && mentions_dry_run_and_apply(&e) => {
            Parsed::Exit {
                code: 64,
                stdout: String::new(),
                stderr: format!("{DRY_RUN_WITH_APPLY}\n"),
            }
        }
        Err(e) => {
            let mut stderr = e.render().to_string();
            if forced(&words) {
                if !stderr.ends_with('\n') {
                    stderr.push('\n');
                }
                stderr.push_str(FORCE_TIP);
                stderr.push('\n');
            }
            Parsed::Exit {
                code: 64,
                stdout: String::new(),
                stderr,
            }
        }
    }
}

/// The command definition, for the test that a conflict is declared and for `--help` snapshots.
pub fn command() -> clap::Command {
    Cli::command()
}

/// Why a selection could not be built from the flags; every variant is a usage error (exit 64).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(pub String);

impl Cli {
    pub fn color_choice(&self) -> ColorChoice {
        if self.no_color {
            return ColorChoice::Never;
        }
        match self.color {
            ColorArg::Auto => ColorChoice::Auto,
            ColorArg::Always => ColorChoice::Always,
            ColorArg::Never => ColorChoice::Never,
        }
    }

    /// The selection of the flags. `registered` is what this build ships; `default_kinds` is what a run
    /// without `--kinds` selects. The check order is the one of `design/ux.md` 1.1: a kind-specific flag
    /// without its kind is raised before any other check.
    pub fn selection(
        &self,
        registered: &[SelectorId],
        default_kinds: &KindSelector,
    ) -> Result<Selection, UsageError> {
        let kinds = match &self.kinds {
            Some(list) => parse_kinds(list)?,
            None => default_kinds.clone(),
        };
        let except = match &self.except {
            Some(list) => parse_kinds(list)?,
            None => KindSelector::default(),
        };
        let (named_kinds, except_ids) = (kinds.clone(), except.clone());
        let mut sel = Selection::new(kinds, except);
        sel.roots.clone_from(&self.root);
        sel.min_age_secs = self.min_age;
        if let Some(days) = self.stale_days {
            sel.stale_days(days);
        }
        sel.min_size_bytes = self.min_size;
        sel.keep_newest = self.keep_newest;
        sel.docker_volumes = self.docker_volumes.map(|v| match v {
            VolumeClass::Anonymous => DockerVolumes::Anonymous,
            VolumeClass::Named => DockerVolumes::Named,
        });
        sel.volumes.clone_from(&self.volume);
        sel.steam_saves = self.steam_saves.map(|DiscardArg::Discard| Discard::Discard);
        sel.worktree_ignored = self
            .worktree_ignored
            .map(|DiscardArg::Discard| Discard::Discard);
        sel.only.clone_from(&self.only);
        sel.validate().map_err(|e| UsageError(e.to_string()))?;
        if self.kinds.is_some() {
            let named = named_kinds.ids().chain(except_ids.ids());
            if let Some(id) = named.into_iter().find(|id| !registered.contains(id)) {
                return Err(UsageError(
                    SelectorError::NotBuilt { id: id.id() }.to_string(),
                ));
            }
        }
        Ok(sel)
    }
}

fn parse_kinds(list: &str) -> Result<KindSelector, UsageError> {
    for word in list.split(',') {
        if word == "all" {
            return Err(UsageError("error: 'all' is not accepted: name the kinds you mean (see 'disk-cleanup kinds'). Nothing was deleted.".into()));
        }
        if let Some(rest) = word.strip_prefix('+') {
            return Err(UsageError(format!("error: '+{rest}' is not accepted: --kinds replaces the default set, so write the whole list, for example --kinds caches,worktrees,build-dirs. Nothing was deleted.")));
        }
    }
    let known: Vec<SelectorId> = SelectorId::ALL
        .iter()
        .chain(SelectorId::TEST_ONLY.iter())
        .copied()
        .collect();
    KindSelector::parse(list, &known).map_err(|e| {
        let text = e.to_string();
        UsageError(if text.starts_with("error: ") {
            text
        } else {
            format!("error: {text}")
        })
    })
}

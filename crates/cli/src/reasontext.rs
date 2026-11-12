//! Reason texts and next actions with their placeholders filled (Story 1.2.3, "Placeholders are filled,
//! never printed"). `SkipReason::reason_text` returns the catalogue text of `design/ux.md` 3.5 verbatim;
//! this module fills `N`, `<x>`, `<process>` and the rest from the facts the owning story's probe produced.
//! A fact that is absent drops its clause, so no placeholder ever reaches the output.

use std::ffi::OsStr;
use std::path::Path;

use dunnage_core::kind::SelectorId;
use dunnage_core::outcome::SkipReason;
use dunnage_core::selector::{format_age, Selection};

use crate::escape::shell_quote;
use crate::units;

/// Facts a probe may have produced for one item. Every field is optional.
#[derive(Debug, Clone, Default)]
pub struct ReasonFacts {
    /// `WorktreeCandidate.unpushed`.
    pub unpushed: Option<u32>,
    /// First referencing container or the compose project (`VolumeReferenced`, `ImageReferenced`).
    pub referrer: Option<String>,
    /// First holder found by the worktree probe: process name and pid (`InUse`).
    pub process: Option<(String, u32)>,
    /// Count and `treestat` size of the ignored files (`HasIgnoredFiles`).
    pub ignored: Option<(u64, u64)>,
    /// Count and size of the ignored files an allowed worktree discards (`--worktree-ignored discard`).
    pub discards: Option<(u64, u64)>,
    /// Size of the duplicate set a build dir belongs to (`KeptAsNewest`, the RISK row note).
    pub duplicates: Option<u32>,
    /// Docker name of the image or volume, or the docker context name.
    pub name: Option<String>,
    /// The path the next action names (`Locked`), or the Steam library.
    pub path: Option<std::path::PathBuf>,
    /// The `<reason>`, `<tool>`, `<version>`, `<signature>`, `<detail>`, `<dir>` and `<OS>` slots.
    pub detail: Option<String>,
}

/// What the texts need besides the facts.
pub struct TextCtx<'a> {
    pub selection: &'a Selection,
    pub selector: SelectorId,
    /// The running binary, for the `sudo` lines.
    pub exe: &'a Path,
    pub ascii_only: bool,
}

fn with(template: &str, slot: &str, value: Option<&str>, fallback: &str) -> String {
    match value {
        Some(v) => template.replace(slot, v),
        None => fallback.to_string(),
    }
}

/// The reason text of `r` with its placeholders filled or their clauses dropped.
pub fn reason(why: SkipReason, facts: &ReasonFacts, ctx: &TextCtx) -> String {
    let text = why.reason_text();
    let detail = facts.detail.as_deref();
    match why {
        // The worktree floor and an unknown age carry their own text (Story 3.1.2).
        SkipReason::TooRecent if ctx.selector == SelectorId::Worktrees && detail.is_some() => {
            detail.unwrap_or_default().to_string()
        }
        SkipReason::TooRecent => {
            let age = ctx
                .selection
                .effective_min_age(ctx.selector)
                .map_or_else(|| "14d".to_string(), format_age);
            text.replace("14d", &age)
        }
        SkipReason::UnpushedCommits => match facts.unpushed {
            Some(1) => "1 unpushed commit".into(),
            Some(count) => format!("{count} unpushed commits"),
            None => "has unpushed commits".into(),
        },
        SkipReason::KeptAsNewest => match facts.duplicates {
            Some(count) => format!("kept as newest of {count} duplicates"),
            None => "kept as the newest of its duplicates".into(),
        },
        SkipReason::VolumeReferenced => with(
            text,
            "<x>",
            facts.referrer.as_deref(),
            "volume used by a container or compose project",
        ),
        SkipReason::ImageReferenced => {
            with(text, "<x>", facts.referrer.as_deref(), "image used by a container")
        }
        SkipReason::DaemonUnavailable => with(
            text,
            "<reason>",
            detail,
            "docker: daemon not reachable",
        ),
        SkipReason::LibraryUnmounted => match &facts.path {
            Some(path) => text.replace("<path>", &path.to_string_lossy()),
            None => "Steam library not mounted".into(),
        },
        // Story 4.3.2: the detail names which job is in the way (balance, scrub or the timer).
        SkipReason::AlreadyRunning => detail.map_or_else(|| text.to_string(), str::to_string),
        SkipReason::ToolMissing => with(text, "<tool>", detail, "a required tool is not installed"),
        SkipReason::NonLocalDockerContext => with(
            text,
            "<name>",
            facts.name.as_deref(),
            "non-local docker context",
        ),
        SkipReason::UnsupportedOnThisOS => with(text, "<OS>", detail, "not supported on this OS"),
        SkipReason::UnrecognisedOutput => {
            with(text, "<tool>", detail, "unrecognised tool output; nothing listed")
        }
        SkipReason::DockerVersionUntested => with(
            text,
            "<version>",
            detail,
            "this docker version is outside the tested range; list-only",
        ),
        SkipReason::LooksLikeDatabase => with(
            text,
            "<signature>",
            detail,
            "volume looks like a database (or is larger than 1 GiB)",
        ),
        SkipReason::InUse => match &facts.process {
            Some((name, pid)) => format!(
                "in use: {name} (pid {pid}) has its working directory or an open file under this worktree"
            ),
            None => "in use by a live process".into(),
        },
        SkipReason::HasIgnoredFiles => match facts.ignored {
            Some((files, bytes)) => format!(
                "holds ignored files that git would delete with it ({files} files, {}, for example .env, *.sqlite)",
                units::size(bytes)
            ),
            None => "holds ignored files that git would delete with it (for example .env, *.sqlite)".into(),
        },
        SkipReason::NotReproducible => match detail {
            Some(why_not) => format!("not reproducible: {why_not}"),
            None => "not reproducible".into(),
        },
        SkipReason::MayHoldSaves => with(
            text,
            "<dir>",
            detail,
            "prefix holds files that may be save games",
        ),
        _ => text.to_string(),
    }
}

fn q(p: &OsStr, ctx: &TextCtx) -> String {
    shell_quote(p, ctx.ascii_only)
}

/// The next action of `r`, filled the same way. Commands that hold a path are quoted by `shell_quote`.
pub fn next_action(r: SkipReason, f: &ReasonFacts, ctx: &TextCtx) -> String {
    let t = r.next_action();
    let exe = q(ctx.exe.as_os_str(), ctx);
    match r {
        SkipReason::Locked => match &f.path {
            Some(p) => format!(
                "git worktree unlock -- {} if intended",
                q(p.as_os_str(), ctx)
            ),
            None => "unlock it with git worktree unlock if intended".into(),
        },
        SkipReason::NeedsRoot => format!("sudo {exe} --apply --kinds {}", ctx.selector.id()),
        SkipReason::LooksLikeDatabase => match &f.name {
            Some(n) => format!(
                "name it with --volume {} if you are sure",
                q(OsStr::new(n), ctx)
            ),
            None => "name it with --volume if you are sure".into(),
        },
        SkipReason::ContentsUnknown => {
            let name = match &f.name {
                Some(n) => format!("--volume {}", q(OsStr::new(n), ctx)),
                None => "name the volume with --volume".into(),
            };
            format!(
                "{name}, or dry run as root: sudo {exe} --kinds docker-volumes (its plan-id is root's, so apply as root too)"
            )
        }
        SkipReason::LocalOnlyImage => match &f.name {
            Some(n) => format!(
                "docker rmi {} by hand if you are sure",
                q(OsStr::new(n), ctx)
            ),
            None => "remove it with docker rmi by hand if you are sure".into(),
        },
        SkipReason::RootRestricted => "run as your user; use sudo only for cache-apt".into(),
        _ => t.to_string(),
    }
}

/// Whether the next action says there is nothing to do.
pub fn is_none_action(action: &str) -> bool {
    action == "none" || action.starts_with("none ") || action.starts_with("none:")
}

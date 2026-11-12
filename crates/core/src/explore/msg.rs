//! Plain data the explorer's scan source, probes and actions exchange with the UI loop (Task 6.2.1a). No
//! behaviour: everything here is `Send` and owned, so a bounded `sync_channel` carries it between threads.

use std::ffi::OsString;
use std::path::PathBuf;

use dunnage_scan::index::{DirEvent, DirInfo};

use crate::ids::AppId;
use crate::kind::SelectorId;
use crate::outcome::SkipReason;
use crate::ports::SharedEstimate;
use crate::snapshot_types::FsSnapshot;

/// `parent` of a root event (the walker's `NO_PARENT`).
pub const NO_PARENT: u64 = dunnage_scan::index::NO_PARENT;

/// Ids of painted events start here, so they never collide with the live walk's dense ids from 0.
pub const PAINT_ID_BASE: u64 = dunnage_scan::index::PAINT_ID_BASE;

/// How the reducer treats a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    /// A scan with nothing painted before it: totals only grow.
    Fresh,
    /// Provisional records from the cache (`painted` events with ids from [`PAINT_ID_BASE`]).
    Paint,
    /// The live pass over a painted tree: events confirm, correct, add or remove painted nodes.
    Revalidate,
}

/// One per-directory fact; mirrors the walker's `DirEvent`. Every id receives exactly one terminal event
/// unless the run is stopped.
#[derive(Debug, Clone)]
pub enum NodeEvent {
    Dir {
        id: u64,
        parent: u64,
        name: OsString,
        dev: u64,
        info: DirInfo,
        /// Child directories that will each get their own event.
        subdirs: u32,
        from_cache: bool,
        painted: bool,
    },
    Unreadable {
        id: u64,
        parent: u64,
        name: OsString,
        errno: i32,
    },
    Boundary {
        id: u64,
        parent: u64,
        name: OsString,
        dev: u64,
    },
    Gone {
        id: u64,
        parent: u64,
        name: OsString,
    },
}

impl NodeEvent {
    pub fn id(&self) -> u64 {
        match self {
            NodeEvent::Dir { id, .. }
            | NodeEvent::Unreadable { id, .. }
            | NodeEvent::Boundary { id, .. }
            | NodeEvent::Gone { id, .. } => *id,
        }
    }

    pub fn parent(&self) -> u64 {
        match self {
            NodeEvent::Dir { parent, .. }
            | NodeEvent::Unreadable { parent, .. }
            | NodeEvent::Boundary { parent, .. }
            | NodeEvent::Gone { parent, .. } => *parent,
        }
    }

    /// Converts the walker's event; `None` for a variant added to the walker after this was written.
    pub fn from_dir_event(e: DirEvent) -> Option<NodeEvent> {
        Some(match e {
            DirEvent::Dir {
                id,
                parent,
                name,
                dev,
                info,
                subdirs,
                from_cache,
                painted,
            } => NodeEvent::Dir {
                id,
                parent,
                name,
                dev,
                info,
                subdirs,
                from_cache,
                painted,
            },
            DirEvent::Unreadable {
                id,
                parent,
                name,
                errno,
            } => NodeEvent::Unreadable {
                id,
                parent,
                name,
                errno,
            },
            DirEvent::Boundary {
                id,
                parent,
                name,
                dev,
            } => NodeEvent::Boundary {
                id,
                parent,
                name,
                dev,
            },
            DirEvent::Gone { id, parent, name } => NodeEvent::Gone { id, parent, name },
            _ => return None,
        })
    }
}

/// Why a truncated walk stopped, so the title can say `(hit --max-seconds N)` or `(stopped by you)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StopCause {
    /// The walk was not truncated.
    #[default]
    None,
    Deadline,
    Cancelled,
}

/// What a source reports at the end of a walk (every event was already delivered).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanSummary {
    pub generation: u32,
    pub truncated: bool,
    pub stop: StopCause,
    pub errors: u64,
    pub dirs_listed: u64,
    pub dirs_cached: u64,
    pub notes: Vec<String>,
}

/// One file of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub name: OsString,
    pub allocated_bytes: u64,
    pub size_bytes: u64,
    /// A second or later link to an inode already seen in this listing: the directory totals counted its
    /// bytes once, at the first link (N-5).
    pub second_link: bool,
}

/// The top N files of a directory by allocated bytes and the totals of the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileRows {
    pub rows: Vec<FileRow>,
    pub smaller_count: u64,
    pub smaller_bytes: u64,
    /// The listing was cut short (cancelled or unreadable).
    pub partial: bool,
}

/// The risk tier a hint badge shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Safe,
    Guarded,
    Destructive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoReason {
    NotShape,
    OutsideRoot,
    InstalledSteamGame,
    InsideBuildOrGit,
}

/// Whether the cleaner would list a path. `Yes`: the engine lists it before its guards. `Shape`: right
/// place and name but the explorer lacks data (git, Steam, the ancestors). Filled by Story 6.4.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hint {
    Yes { selector: SelectorId, tier: Tier },
    Shape { selector: SelectorId, tier: Tier },
    No(NoReason),
}

/// Why a path cannot be marked. Texts live in `refusal.rs` (Story 6.4.1). The last five variants before
/// `Engine` are the rules M3, M5 and M6 of Story 6.6.1; `BindMountBelow` is M3's second refusal (ADR-006).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkRefusal {
    NotAnItem,
    OutsideRoot,
    InsideBuildOrGit,
    SymlinkRow,
    ContainsItems(u32),
    LimitReached,
    PathTooLong,
    StillScanning,
    /// M3: the path is a mount point (or the boundary row of another filesystem).
    MountPoint,
    /// M3: a same-device bind mount at this path sits below the directory; the engine cannot see it.
    BindMountBelow(PathBuf),
    /// M5: the path is, holds or lies inside the tool's own state.
    OwnState,
    /// M6: the path lies inside this already marked path.
    InsideMarked(PathBuf),
    /// M6: the path holds this many marked items.
    ContainsMarked(u32),
    /// Marks are folders only: `delete_tree` removes directories (review C3).
    NotADirectory,
    Engine(SkipReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamLibraryFacts {
    pub root: PathBuf,
    pub mounted: bool,
    pub installed: Vec<AppId>,
    pub install_dirs: Vec<OsString>,
}

/// What the hint needs and a pure function cannot read: the resolved cache paths and the Steam libraries.
/// `steam` is `None` when it was not loaded or failed, which makes Steam rows `Shape`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassifyFacts {
    pub cache_paths: Vec<PathBuf>,
    pub steam: Option<Vec<SteamLibraryFacts>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRow {
    pub path: PathBuf,
    pub kind: String,
    pub est_bytes: u64,
    pub status: String,
}

/// The plan the marks would run; never carries a `Next:` line (a TUI-wrapped command would not paste).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPreview {
    pub plan_id: String,
    pub saved: bool,
    pub rows: Vec<PreviewRow>,
    pub est_bytes: u64,
    pub scope_line: String,
    pub guards_line: String,
}

/// Everything a scan source or a worker sends to the UI loop.
#[derive(Debug, Clone)]
pub enum ScanMsg {
    Batch {
        generation: u32,
        mode: ScanMode,
        events: Vec<NodeEvent>,
    },
    Finished(ScanSummary),
    SourceFailed(String),
    Headroom(FsSnapshot),
    FileRows {
        dir: PathBuf,
        rows: FileRows,
    },
    Shared {
        dir: PathBuf,
        estimate: SharedEstimate,
    },
    Facts(ClassifyFacts),
    Preview(PlanPreview),
    /// The planner could not run (a mark still being scanned, every kind down): the line for the footer.
    PreviewFailed(String),
}

/// What happened to one planned item of an explorer delete (Story 6.6.4 shows these in the result list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteItemOutcome {
    /// Removed; the measured bytes freed.
    Removed { bytes: u64 },
    /// Left alone with a catalogue reason (`changed since the plan`, ...).
    Skipped { reason: String },
    /// The delete started and failed.
    Failed { reason: String },
}

/// The result line of one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteItemResult {
    pub path: PathBuf,
    pub outcome: DeleteItemOutcome,
}

/// How a delete ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeleteSummary {
    pub removed: usize,
    pub bytes: u64,
    /// A signal stopped it; `run_end` says `interrupted`.
    pub interrupted: bool,
}

/// Everything the worker of an explorer delete sends to the UI loop, in this order: `Progress` and `Item` per
/// item, then exactly one of `Done` or `Refused`. `Refused` means nothing was deleted (the stamp, the lock,
/// `Changed since you confirmed ...`, an unwritable audit log); its text is the whole message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteMsg {
    /// Item `index` (1-based) of `total` is starting: the title reads `DELETING 1 of 2`.
    Progress {
        index: usize,
        total: usize,
    },
    Item(DeleteItemResult),
    Done(DeleteSummary),
    Refused(String),
}

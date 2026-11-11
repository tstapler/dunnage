//! Port traits (Story 1.1.2): everything `core` needs from the outside world goes through one of these.
//! All of them are `Send + Sync` so one shared [`Env`] serves the executor's scoped worker threads.
//!
//! The command allowlist ([`classify`]) is the whole list of what the tool may run. A new command is added
//! by editing `TABLE`, which is a contract-changing edit (reviewed with the independent test author).

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::time::Duration;

use crate::allowed::WorktreeRemoval;
use crate::apply_token::ApplyToken;
use crate::audit_types::{Intent, ItemResult, RunEnd, RunStart};
use crate::delete_types::{AbortFlag, DeleteError, DirEntryInfo, StatInfo};
use crate::dirs::Dirs;
use crate::plan_state::{SaveOutcome, SavedPlan, SavedSelection};
use crate::platform::{MountEntry, Platform, Tri};
use crate::reasons::FatalError;
use crate::snapshot_types::{FsSnapshot, Settled};

/// An external command as argv. `mutating` is an author label only: [`classify`] ignores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub mutating: bool,
}

impl Cmd {
    pub fn new<P, I, A>(program: P, args: I, mutating: bool) -> Cmd
    where
        P: Into<OsString>,
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        Cmd {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            mutating,
        }
    }
}

/// Which door a command may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    ReadOnly,
    Mutating,
}

/// Output of a command that exited 0 (a non-zero exit is `DeleteError::CommandFailed`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CmdOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CmdOutput {
    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// One argv token of an allowlist shape: an exact literal, one free value (non-empty, no leading `-`, no NUL)
/// or a value with a fixed prefix.
enum T {
    L(&'static str),
    V,
    P(&'static str),
}
use T::{L, P, V};

use Class::{Mutating as M, ReadOnly as R};

/// (program, class, argv shape after the optional `-C <repo>` of git). The program is matched byte-exact and
/// case-sensitively against a bare name: `/usr/bin/git`, `Git`, `GIT` and `/usr/lib/git-core/git-worktree`
/// are not in the table and are refused. `git worktree remove` is not a row: it has one door,
/// [`CommandRunner::remove_worktree`].
const TABLE: &[(&str, Class, &[T])] = &[
    ("docker", M, &[L("image"), L("rm"), V]),
    ("docker", M, &[L("volume"), L("rm"), V]),
    (
        "docker",
        M,
        &[
            L("builder"),
            L("prune"),
            L("--force"),
            L("--filter"),
            P("until="),
        ],
    ),
    ("btrfs", M, &[L("balance"), L("start"), L("-dusage=0"), V]),
    ("btrfs", M, &[L("balance"), L("start"), L("-dusage=5"), V]),
    ("btrfs", M, &[L("balance"), L("start"), L("-dusage=10"), V]),
    ("btrfs", M, &[L("balance"), L("start"), L("-dusage=20"), V]),
    ("btrfs", M, &[L("balance"), L("start"), L("-dusage=40"), V]),
    ("fstrim", M, &[L("-v"), V]),
    ("go", M, &[L("clean"), L("-cache")]),
    ("pip", M, &[L("cache"), L("purge")]),
    ("pip3", M, &[L("cache"), L("purge")]),
    ("brew", M, &[L("cleanup")]),
    ("xcrun", M, &[L("simctl"), L("delete"), L("unavailable")]),
    ("apt-get", M, &[L("clean")]),
    // read-only, used by `run` only
    ("docker", R, &[L("ps")]),
    (
        "docker",
        R,
        &[L("ps"), L("-a"), L("--filter"), P("volume=")],
    ),
    ("docker", R, &[L("ps"), L("-a"), L("--format"), V]),
    (
        "docker",
        R,
        &[
            L("ps"),
            L("-a"),
            L("--filter"),
            P("volume="),
            L("--format"),
            V,
        ],
    ),
    ("docker", R, &[L("info")]),
    ("docker", R, &[L("version"), L("--format"), V]),
    ("docker", R, &[L("images"), L("--format"), V]),
    (
        "docker",
        R,
        &[L("images"), L("--filter"), V, L("--format"), V],
    ),
    ("docker", R, &[L("volume"), L("ls"), L("--format"), V]),
    (
        "docker",
        R,
        &[L("system"), L("df"), L("-v"), L("--format"), V],
    ),
    ("lsof", R, &[L("-F"), L("n"), L("+D"), V]),
    ("docker", R, &[L("images")]),
    ("docker", R, &[L("volume"), L("ls")]),
    ("docker", R, &[L("volume"), L("inspect"), V]),
    ("docker", R, &[L("system"), L("df")]),
    ("docker", R, &[L("context"), L("show")]),
    ("btrfs", R, &[L("balance"), L("status"), V]),
    ("go", R, &[L("env"), V]),
    ("pip", R, &[L("cache"), L("dir")]),
    ("pip3", R, &[L("cache"), L("dir")]),
    ("git", R, &[L("worktree"), L("list"), L("--porcelain")]),
    (
        "git",
        R,
        &[L("worktree"), L("list"), L("--porcelain"), L("-z")],
    ),
    (
        "git",
        R,
        &[
            L("status"),
            L("--porcelain=v1"),
            L("--untracked-files=normal"),
        ],
    ),
    ("git", R, &[L("rev-parse"), L("--git-common-dir")]),
    ("git", R, &[L("branch"), L("-r"), L("--contains"), V]),
    ("git", R, &[L("log"), L("-1"), L("--format=%ct")]),
];

fn val(a: &str) -> bool {
    !a.is_empty() && !a.starts_with('-') && !a.contains('\0')
}

fn shape_matches(shape: &[T], args: &[&str]) -> bool {
    shape.len() == args.len()
        && shape.iter().zip(args).all(|(t, a)| match t {
            L(s) => a == s,
            V => val(a),
            P(p) => a
                .strip_prefix(p)
                .is_some_and(|rest| !rest.is_empty() && !rest.contains('\0')),
        })
}

/// The one classifier. `None` means refused. The `mutating` label on the `Cmd` is ignored.
/// `run` accepts only `ReadOnly` rows, `run_mutating` only `Mutating` rows.
pub fn classify(c: &Cmd) -> Option<Class> {
    let prog = c.program.to_str()?;
    if prog.is_empty() || prog.contains('/') || prog.contains('\\') {
        return None;
    }
    let args: Vec<&str> = c.args.iter().map(|a| a.to_str()).collect::<Option<_>>()?;
    // git: at most one leading `-C <repo>`; every other global option (-c, --exec-path, --git-dir, ...) is refused
    let rest: &[&str] = if prog == "git" {
        match args.as_slice() {
            ["-C", repo, tail @ ..] if val(repo) => tail,
            _ => &args,
        }
    } else {
        &args
    };
    TABLE
        .iter()
        .find(|(p, _, shape)| *p == prog && shape_matches(shape, rest))
        .map(|(_, k, _)| *k)
}

/// `(mutating rows, read-only rows)` of the table, for the row-count test.
pub fn allowlist_row_counts() -> (usize, usize) {
    let m = TABLE.iter().filter(|(_, k, _)| *k == M).count();
    (m, TABLE.len() - m)
}

/// Runs external commands: argv, no shell. Failures are `DeleteError`s: `Refused` (not an allowlist row of the
/// right class), `Timeout`, `CommandFailed { status, message }`, `Io(errno)` (spawn failure).
///
/// There is no `run_mutating` without a token, so plan-mode code cannot even spell the call:
///
/// ```compile_fail
/// use disk_cleanup_core::ports::{Cmd, CommandRunner};
/// fn plan_mode(r: &dyn CommandRunner) {
///     let cmd = Cmd::new("docker", ["image", "rm", "sha256:ab12"], true);
///     let _ = r.run_mutating(&cmd);
/// }
/// ```
pub trait CommandRunner: Send + Sync {
    /// Whether `program` is on `PATH` (replaces `which`), without spawning it.
    fn exists(&self, program: &str) -> bool;
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, DeleteError>;
    fn run_mutating(&self, cmd: &Cmd, proof: &ApplyToken) -> Result<CmdOutput, DeleteError>;
    /// The only way to run `git worktree remove`: the argument can be built only from an `Allowed`.
    fn remove_worktree(
        &self,
        w: &WorktreeRemoval,
        proof: &ApplyToken,
    ) -> Result<CmdOutput, DeleteError>;
}

/// Plan-mode runner: forwards read-only rows to `inner` and refuses everything that mutates.
#[derive(Debug)]
pub struct DryRunRunner<R>(pub R);

impl<R: CommandRunner> CommandRunner for DryRunRunner<R> {
    fn exists(&self, program: &str) -> bool {
        self.0.exists(program)
    }

    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, DeleteError> {
        match classify(cmd) {
            Some(Class::ReadOnly) => self.0.run(cmd),
            _ => Err(DeleteError::Refused),
        }
    }

    fn run_mutating(&self, _cmd: &Cmd, _proof: &ApplyToken) -> Result<CmdOutput, DeleteError> {
        Err(DeleteError::Refused)
    }

    fn remove_worktree(
        &self,
        _w: &WorktreeRemoval,
        _proof: &ApplyToken,
    ) -> Result<CmdOutput, DeleteError> {
        Err(DeleteError::Refused)
    }
}

/// Read-only filesystem access for guards and discovery. Deletion never goes through this port.
pub trait Fs: Send + Sync {
    /// `lstat`: never follows a final symlink.
    fn lstat(&self, path: &Path) -> io::Result<StatInfo>;
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntryInfo>>;
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
}

pub trait Clock: Send + Sync {
    fn now_epoch(&self) -> i64;
}

/// `Idle` / `Running` / `Unknown` for a build directory. `Unknown` skips (fail closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunState {
    Idle,
    Running,
    Unknown,
}

/// Every process or host fact a guard needs. No story outside `adapters/probe*.rs` reads `/proc`, calls
/// `geteuid` or inspects the OS.
pub trait SystemProbe: Send + Sync {
    fn euid(&self) -> u32;
    fn platform(&self) -> Platform;
    fn process_named(&self, name: &str) -> Tri;
    fn run_state(&self, path: &Path) -> RunState;
    /// The second element is the process name and pid of the first holder, for the `InUse` text.
    fn cwd_in_use(&self, path: &Path) -> (Tri, Option<(String, u32)>);
    /// Mount point, device field and mount id. The device field is never compared with an `st_dev`
    /// (spike 0.2.1): the table answers only "is this path a mount point".
    fn mounts(&self) -> io::Result<Vec<MountEntry>>;
}

/// One extent that carries `FIEMAP_EXTENT_SHARED`, keyed by where it lives on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SharedExtent {
    pub physical: u64,
    pub length: u64,
}

/// The shared extents of one probed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemExtents {
    pub path: std::path::PathBuf,
    pub shared: Vec<SharedExtent>,
}

/// Why the probe could not measure; attribution then falls back to the plain `st_blocks` sum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    NotBtrfs,
    UnsupportedOnThisOS,
    /// `ENOTTY` or `EOPNOTSUPP` from the ioctl.
    IoctlUnsupported,
    BudgetExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtentAttribution {
    Unavailable(Unavailable),
    /// One entry per probed regular file; a path that is gone or not a regular file has none.
    Measured(Vec<ItemExtents>),
}

/// Shared-extent attribution (ADR-004 outcome 3). Read-only; the delete never depends on it.
pub trait ExtentProbe: Send + Sync {
    /// `budget` is the most files the probe may open; more than that is `Unavailable(BudgetExhausted)`.
    fn attribute(&self, items: &[&Path], budget: usize) -> ExtentAttribution;

    /// The explorer's info-pane estimate: bytes of `SHARED` extents among the `top_n` largest files of
    /// `dir`, stopping when `budget` of wall time is spent. An estimate, labelled so everywhere it is shown
    /// (compressed extents count uncompressed; a snapshot counts as sharing). The default is
    /// `Unavailable(UnsupportedOnThisOS)` so existing probes keep compiling.
    fn shared_bytes(&self, _dir: &Path, _top_n: usize, _budget: Duration) -> SharedEstimate {
        SharedEstimate::Unavailable(Unavailable::UnsupportedOnThisOS)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedEstimate {
    Unavailable(Unavailable),
    Estimated {
        /// Bytes of the probed files' extents that carry `FIEMAP_EXTENT_SHARED`.
        bytes: u64,
        /// Files whose extents were read (`<= top_n`).
        files_probed: u64,
        /// Files that were selected but could not be opened.
        unreadable: u64,
        /// Allocated bytes of the probed files.
        probed_usage: u64,
        /// Allocated bytes of every regular file the listing reached before the budget ran out.
        dir_usage: u64,
        /// The time budget ended the listing or the probing early.
        partial: bool,
    },
}

impl SharedEstimate {
    /// The info-pane line, always worded as an estimate; `None` when the probe was unavailable.
    #[must_use]
    pub fn pane_text(&self) -> Option<String> {
        let Self::Estimated {
            bytes,
            files_probed,
            probed_usage,
            dir_usage,
            partial,
            ..
        } = *self
        else {
            return None;
        };
        let share = if dir_usage == 0 {
            0
        } else {
            probed_usage.saturating_mul(100) / dir_usage
        };
        let scope = if files_probed == 1 {
            "the largest file".to_string()
        } else {
            format!("the {files_probed} largest files")
        };
        let cut = if partial {
            " (stopped at the time budget)"
        } else {
            ""
        };
        Some(format!(
            "est. {} of {scope} ({share}% of usage) also referenced elsewhere; a delete may free less{cut}",
            human_bytes(bytes)
        ))
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

pub trait SpaceMeter: Send + Sync {
    fn snapshot(&self, path: &Path) -> FsSnapshot;
    fn settle(&self, path: &Path, timeout: Duration) -> Settled;
}

/// Append-only audit log; an `Err` from `intent` aborts before the next delete.
pub trait AuditSink: Send + Sync {
    fn run_start(&self, rec: &RunStart) -> io::Result<()>;
    fn intent(&self, rec: &Intent) -> io::Result<()>;
    fn result(&self, rec: &ItemResult) -> io::Result<()>;
    fn run_end(&self, rec: &RunEnd) -> io::Result<()>;
}

pub trait RunGuard: Send + Sync {
    fn acquire(&self) -> Result<(), FatalError>;
    fn abort_flag(&self) -> &AbortFlag;
    /// The signal number (2 or 15) that set the abort flag.
    fn interrupted(&self) -> Option<i32>;
}

/// `last_plan`: a merged set of saved selections (see [`merge_saved`]).
pub trait PlanStore: Send + Sync {
    fn load_last(&self) -> Option<SavedPlan>;
    fn save_merged(&self, new: Vec<SavedSelection>, euid: u32, now: i64) -> SaveOutcome;
}

/// Single injection point: every capability the executor needs.
pub struct Env<'a> {
    pub fs: &'a dyn Fs,
    pub runner: &'a dyn CommandRunner,
    pub clock: &'a dyn Clock,
    pub probe: &'a dyn SystemProbe,
    pub space: &'a dyn SpaceMeter,
    pub audit: &'a dyn AuditSink,
    pub guard: &'a dyn RunGuard,
    pub plans: &'a dyn PlanStore,
    pub dirs: Dirs,
}

pub const SAVED_ENTRY_MAX_BYTES: usize = 32_768;
pub const SAVED_FILE_MAX_BYTES: usize = 524_288;
pub const SAVED_MAX_ENTRIES: usize = 32;
pub const SAVED_TTL_SECS: i64 = 86_400;

fn json_len<T: serde::Serialize>(v: &T) -> usize {
    serde_json::to_vec(v).map_or(usize::MAX, |b| b.len())
}

/// Cut `e.items` so the serialized entry is at most [`SAVED_ENTRY_MAX_BYTES`].
fn cut_entry(e: &mut SavedSelection) {
    if json_len(e) <= SAVED_ENTRY_MAX_BYTES {
        return;
    }
    let mut bare = e.clone();
    bare.items.clear();
    bare.items_truncated = true;
    let mut used = json_len(&bare);
    let mut keep = 0;
    for key in &e.items {
        let add = json_len(key) + usize::from(keep > 0);
        if used + add > SAVED_ENTRY_MAX_BYTES {
            break;
        }
        used += add;
        keep += 1;
    }
    e.items.truncate(keep);
    e.items_truncated = true;
}

/// The set rule for `last_plan` (pure; `FakePlanStore` and the real store both call it).
///
/// Entries older than 24 h or dated in the future are dropped; a new entry replaces the existing one with the
/// same `(plan_id, euid)` (its `saved_at` becomes `now`, its `euid` the caller's); entries of other euids are
/// kept untouched; every entry is cut to 32 KiB; at most 32 entries and 512 KiB are kept, evicting the oldest
/// first and never the current run's own entries. The result is ordered oldest first.
pub fn merge_saved(
    existing: Vec<SavedSelection>,
    new: Vec<SavedSelection>,
    euid: u32,
    now: i64,
) -> Vec<SavedSelection> {
    let mut current: Vec<SavedSelection> = Vec::with_capacity(new.len());
    for mut e in new {
        e.saved_at = now;
        e.euid = euid;
        if let Some(prev) = current.iter_mut().find(|c| c.plan_id == e.plan_id) {
            *prev = e;
        } else {
            current.push(e);
        }
    }
    let kept: Vec<SavedSelection> = existing
        .into_iter()
        .filter(|e| e.saved_at <= now && now - e.saved_at <= SAVED_TTL_SECS)
        .filter(|e| {
            !current
                .iter()
                .any(|c| c.plan_id == e.plan_id && c.euid == e.euid)
        })
        .collect();
    let mut all: Vec<(SavedSelection, bool)> = kept
        .into_iter()
        .map(|e| (e, false))
        .chain(current.into_iter().map(|e| (e, true)))
        .collect();
    for (e, _) in &mut all {
        cut_entry(e);
    }
    // Stable: ties keep existing entries before the current run's.
    all.sort_by_key(|(e, _)| e.saved_at);
    let fits = |all: &[(SavedSelection, bool)]| {
        let size =
            "{\"entries\":[]}".len() + all.iter().map(|(e, _)| json_len(e) + 1).sum::<usize>();
        all.len() <= SAVED_MAX_ENTRIES && size <= SAVED_FILE_MAX_BYTES
    };
    while !fits(&all) {
        match all.iter().position(|(_, is_current)| !is_current) {
            Some(i) => {
                all.remove(i);
            }
            None => break,
        }
    }
    all.into_iter().map(|(e, _)| e).collect()
}

/// The argv a `WorktreeRemoval` turns into. Built only here and never classified: `run_mutating` refuses it.
pub fn worktree_remove_argv(w: &WorktreeRemoval) -> Vec<OsString> {
    vec![
        OsString::from("-C"),
        w.main_repo().as_os_str().to_owned(),
        OsString::from("worktree"),
        OsString::from("remove"),
        w.path().as_os_str().to_owned(),
    ]
}

/// Lossy argv rendering for panic and log messages.
pub fn render_argv(program: &OsStr, args: &[OsString]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(OsString::as_os_str))
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod shared_estimate_text {
    use super::{SharedEstimate, Unavailable};

    const GIB: u64 = 1 << 30;

    #[test]
    fn pane_text_should_say_estimate_the_scope_the_share_and_that_a_delete_may_free_less() {
        let e = SharedEstimate::Estimated {
            bytes: GIB + GIB / 5,
            files_probed: 10,
            unreadable: 0,
            probed_usage: 38,
            dir_usage: 100,
            partial: false,
        };
        assert_eq!(
            e.pane_text().unwrap(),
            "est. 1.2 GiB of the 10 largest files (38% of usage) also referenced elsewhere; a delete may free less"
        );
    }

    #[test]
    fn pane_text_should_flag_a_partial_run_and_be_none_when_unavailable() {
        let e = SharedEstimate::Estimated {
            bytes: 0,
            files_probed: 1,
            unreadable: 0,
            probed_usage: 0,
            dir_usage: 0,
            partial: true,
        };
        assert!(e
            .pane_text()
            .unwrap()
            .ends_with("(stopped at the time budget)"));
        assert_eq!(
            SharedEstimate::Unavailable(Unavailable::NotBtrfs).pane_text(),
            None
        );
    }
}

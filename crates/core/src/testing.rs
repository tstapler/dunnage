//! Test doubles for the ports, behind the test-support feature (Story 1.1.2). No test sleeps: time is a
//! settable [`FakeClock`]; every fake is `Send + Sync` so one `&Env` can be shared across scoped threads.
//!
//! The fakes fail loudly: [`FakeCommandRunner`] panics, naming the argv, when a command arrives that no
//! expectation covers.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::allowed::WorktreeRemoval;
use crate::apply_token::ApplyToken;
use crate::audit_types::{AuditRecord, Intent, ItemResult, RunEnd, RunStart};
use crate::delete_types::{AbortFlag, DeleteError, DirEntryInfo, FileKind, StatInfo};
use crate::ids::Identity;
use crate::plan_state::{SaveOutcome, SavedPlan, SavedSelection};
use crate::platform::{MountEntry, Platform, Tri};
use crate::ports::{
    classify, merge_saved, render_argv, worktree_remove_argv, AuditSink, Class, Clock, Cmd,
    CmdOutput, CommandRunner, Fs, PlanStore, RunGuard, RunState, SpaceMeter, SystemProbe,
};
use crate::reasons::FatalError;
use crate::snapshot_types::{FsSnapshot, Settled};

pub use crate::ports::DryRunRunner;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A shared, ordered log several fakes can append to, so a test can assert the order of calls across ports
/// (for example `run_start`, `intent`, `result`, `run_end`).
#[derive(Debug, Clone, Default)]
pub struct EventLog(Arc<Mutex<Vec<String>>>);

impl EventLog {
    pub fn new() -> EventLog {
        EventLog::default()
    }

    pub fn push(&self, event: impl Into<String>) {
        lock(&self.0).push(event.into());
    }

    pub fn events(&self) -> Vec<String> {
        lock(&self.0).clone()
    }
}

fn note(log: &Mutex<Option<EventLog>>, event: impl FnOnce() -> String) {
    if let Some(l) = lock(log).as_ref() {
        l.push(event());
    }
}

// ---------------------------------------------------------------- command runner

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    Run,
    RunMutating,
    RemoveWorktree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCall {
    pub kind: CallKind,
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl RecordedCall {
    pub fn rendered(&self) -> String {
        render_argv(&self.program, &self.args)
    }
}

type Reply = Result<CmdOutput, DeleteError>;

#[derive(Debug, Default)]
struct RunnerState {
    expect: Vec<(Vec<OsString>, VecDeque<Reply>)>,
    absent: Vec<String>,
    calls: Vec<RecordedCall>,
    /// Argvs whose `run_mutating_unless_aborted` blocks until the abort flag is set.
    blocking: Vec<Vec<OsString>>,
}

/// Scripted command runner. `on(argv, reply)` registers a reply for an exact argv (program first); several
/// replies for one argv are returned in order and the last one repeats. Every entry point classifies first,
/// so a mislabelled or non-allowlisted command is `Err(Refused)` and never recorded. A classified command
/// with no expectation panics with its argv (the worktree door too: its argv is `git -C <repo> worktree
/// remove <path>`).
#[derive(Debug, Default)]
#[must_use]
pub struct FakeCommandRunner {
    state: Mutex<RunnerState>,
    log: Mutex<Option<EventLog>>,
}

impl FakeCommandRunner {
    pub fn new() -> FakeCommandRunner {
        FakeCommandRunner::default()
    }

    pub fn with_log(self, log: &EventLog) -> FakeCommandRunner {
        *lock(&self.log) = Some(log.clone());
        self
    }

    pub fn on(&self, argv: &[&str], reply: Reply) -> &FakeCommandRunner {
        let key: Vec<OsString> = argv.iter().map(OsString::from).collect();
        let mut st = lock(&self.state);
        match st.expect.iter_mut().find(|(k, _)| *k == key) {
            Some((_, q)) => q.push_back(reply),
            None => st.expect.push((key, VecDeque::from([reply]))),
        }
        self
    }

    /// Makes `run_mutating_unless_aborted` of `argv` block, as a command running in the kernel does, until the
    /// abort flag it is given is set; it then returns `Ok(None)`. The call is recorded when it starts.
    pub fn block_until_aborted(&self, argv: &[&str]) -> &FakeCommandRunner {
        lock(&self.state)
            .blocking
            .push(argv.iter().map(OsString::from).collect());
        self
    }

    /// Registers `program` as not installed: `exists` is false and nothing is spawned.
    pub fn absent(&self, program: &str) -> &FakeCommandRunner {
        lock(&self.state).absent.push(program.to_string());
        self
    }

    pub fn calls(&self) -> Vec<RecordedCall> {
        lock(&self.state).calls.clone()
    }

    pub fn calls_of(&self, kind: CallKind) -> Vec<RecordedCall> {
        self.calls()
            .into_iter()
            .filter(|c| c.kind == kind)
            .collect()
    }

    pub fn mutating_calls(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.kind != CallKind::Run)
            .count()
    }

    fn answer(&self, kind: CallKind, program: OsString, args: Vec<OsString>) -> Reply {
        let rendered = render_argv(&program, &args);
        let mut argv = vec![program.clone()];
        argv.extend(args.iter().cloned());
        lock(&self.state).calls.push(RecordedCall {
            kind,
            program,
            args,
        });
        note(&self.log, || format!("{kind:?} {rendered}"));
        let mut st = lock(&self.state);
        let Some((_, q)) = st.expect.iter_mut().find(|(k, _)| *k == argv) else {
            drop(st);
            panic!("FakeCommandRunner: unexpected {kind:?} command: {rendered}");
        };
        let reply = if q.len() > 1 {
            q.pop_front()
        } else {
            q.front().cloned()
        };
        reply.unwrap_or_else(|| Ok(CmdOutput::default()))
    }
}

impl CommandRunner for FakeCommandRunner {
    fn exists(&self, program: &str) -> bool {
        !lock(&self.state).absent.iter().any(|a| a == program)
    }

    fn run(&self, cmd: &Cmd) -> Reply {
        if classify(cmd) != Some(Class::ReadOnly) {
            return Err(DeleteError::Refused);
        }
        self.answer(CallKind::Run, cmd.program.clone(), cmd.args.clone())
    }

    fn run_mutating(&self, cmd: &Cmd, _proof: &ApplyToken) -> Reply {
        if classify(cmd) != Some(Class::Mutating) {
            return Err(DeleteError::Refused);
        }
        self.answer(CallKind::RunMutating, cmd.program.clone(), cmd.args.clone())
    }

    fn remove_worktree(&self, w: &WorktreeRemoval, _proof: &ApplyToken) -> Reply {
        self.answer(
            CallKind::RemoveWorktree,
            OsString::from("git"),
            worktree_remove_argv(w),
        )
    }

    fn run_mutating_unless_aborted(
        &self,
        cmd: &Cmd,
        proof: &ApplyToken,
        abort: &AbortFlag,
    ) -> Result<Option<CmdOutput>, DeleteError> {
        let mut argv = vec![cmd.program.clone()];
        argv.extend(cmd.args.iter().cloned());
        let blocks = classify(cmd) == Some(Class::Mutating)
            && lock(&self.state).blocking.iter().any(|b| *b == argv);
        if !blocks {
            return self.run_mutating(cmd, proof).map(Some);
        }
        lock(&self.state).calls.push(RecordedCall {
            kind: CallKind::RunMutating,
            program: cmd.program.clone(),
            args: cmd.args.clone(),
        });
        note(&self.log, || {
            format!(
                "RunMutating {} (blocked)",
                render_argv(&cmd.program, &cmd.args)
            )
        });
        let give_up = std::time::Instant::now() + Duration::from_secs(30);
        while !abort.is_set() {
            assert!(
                std::time::Instant::now() < give_up,
                "FakeCommandRunner: a blocked command was never aborted"
            );
            std::thread::yield_now();
        }
        Ok(None)
    }
}

// ---------------------------------------------------------------- clock

#[derive(Debug, Default)]
pub struct FakeClock(AtomicI64);

impl FakeClock {
    pub fn at(now_epoch: i64) -> FakeClock {
        FakeClock(AtomicI64::new(now_epoch))
    }

    pub fn set(&self, now_epoch: i64) {
        self.0.store(now_epoch, Ordering::SeqCst);
    }

    pub fn advance(&self, secs: i64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_epoch(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------- fs

#[derive(Debug, Clone)]
enum FakeNode {
    Dir,
    File(String),
    Symlink,
}

/// In-memory read-only filesystem with per-path errno injection. Parents are not created implicitly: add
/// every directory a test lists.
#[derive(Debug, Default)]
pub struct FakeFs {
    nodes: HashMap<PathBuf, FakeNode>,
    faults: HashMap<PathBuf, i32>,
}

impl FakeFs {
    pub const DEV: u64 = 1;

    pub fn new() -> FakeFs {
        FakeFs::default()
    }

    pub fn add_dir(&mut self, p: impl Into<PathBuf>) -> &mut FakeFs {
        self.nodes.insert(p.into(), FakeNode::Dir);
        self
    }

    pub fn add_file(&mut self, p: impl Into<PathBuf>, content: &str) -> &mut FakeFs {
        self.nodes
            .insert(p.into(), FakeNode::File(content.to_string()));
        self
    }

    pub fn add_symlink(&mut self, p: impl Into<PathBuf>) -> &mut FakeFs {
        self.nodes.insert(p.into(), FakeNode::Symlink);
        self
    }

    /// Every operation on `p` fails with this raw errno.
    pub fn fail(&mut self, p: impl Into<PathBuf>, errno: i32) -> &mut FakeFs {
        self.faults.insert(p.into(), errno);
        self
    }

    fn check(&self, p: &Path) -> io::Result<&FakeNode> {
        if let Some(e) = self.faults.get(p) {
            return Err(io::Error::from_raw_os_error(*e));
        }
        self.nodes
            .get(p)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    /// Stable per path, so two `lstat` calls agree.
    fn ino_of(p: &Path) -> u64 {
        p.as_os_str()
            .as_encoded_bytes()
            .iter()
            .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
                (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
            })
    }
}

fn kind_of(n: &FakeNode) -> FileKind {
    match n {
        FakeNode::Dir => FileKind::Dir,
        FakeNode::File(_) => FileKind::File,
        FakeNode::Symlink => FileKind::Symlink,
    }
}

impl Fs for FakeFs {
    fn lstat(&self, path: &Path) -> io::Result<StatInfo> {
        let n = self.check(path)?;
        let size = match n {
            FakeNode::File(c) => c.len() as u64,
            _ => 0,
        };
        Ok(StatInfo {
            ident: Identity {
                dev: FakeFs::DEV,
                ino: FakeFs::ino_of(path),
            },
            kind: kind_of(n),
            mode: if matches!(n, FakeNode::Dir) {
                0o755
            } else {
                0o644
            },
            nlink: 1,
            blocks: size.div_ceil(512),
            size,
            mtime_secs: 0,
            mtime_nsec: 0,
            mnt_id: None,
        })
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntryInfo>> {
        if !matches!(self.check(path)?, FakeNode::Dir) {
            return Err(io::Error::from_raw_os_error(20));
        }
        let mut out: Vec<DirEntryInfo> = self
            .nodes
            .iter()
            .filter(|(p, _)| p.parent() == Some(path))
            .filter_map(|(p, n)| {
                Some(DirEntryInfo {
                    name: p.file_name()?.to_owned(),
                    kind: kind_of(n),
                    ino: FakeFs::ino_of(p),
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        match self.check(path)? {
            FakeNode::File(c) => Ok(c.clone()),
            FakeNode::Dir => Err(io::Error::from_raw_os_error(21)),
            FakeNode::Symlink => Err(io::Error::from_raw_os_error(40)),
        }
    }
}

// ---------------------------------------------------------------- space meter

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeterCall {
    Snapshot(PathBuf),
    Settle(PathBuf),
}

/// Returns the scripted snapshots in order (the last repeats) and the scripted `Settled`.
#[derive(Debug)]
#[must_use]
pub struct FakeSpaceMeter {
    snapshots: Mutex<VecDeque<FsSnapshot>>,
    settled: Settled,
    calls: Mutex<Vec<MeterCall>>,
    log: Mutex<Option<EventLog>>,
    /// `try_snapshot` fails with this errno when set.
    fail_errno: Mutex<Option<i32>>,
}

impl FakeSpaceMeter {
    pub fn new(snapshots: Vec<FsSnapshot>, settled: Settled) -> FakeSpaceMeter {
        FakeSpaceMeter {
            snapshots: Mutex::new(snapshots.into()),
            settled,
            calls: Mutex::new(Vec::new()),
            log: Mutex::new(None),
            fail_errno: Mutex::new(None),
        }
    }

    /// Makes `try_snapshot` fail with `errno` (`snapshot` keeps answering).
    pub fn failing(self, errno: i32) -> FakeSpaceMeter {
        *lock(&self.fail_errno) = Some(errno);
        self
    }

    pub fn with_log(self, log: &EventLog) -> FakeSpaceMeter {
        *lock(&self.log) = Some(log.clone());
        self
    }

    pub fn stable() -> Settled {
        Settled {
            stable: true,
            data_bytes_used: None,
            samples: 2,
            waited_ms: 0,
        }
    }

    pub fn plain_snapshot(path: &str, used_bytes: u64, total_bytes: u64) -> FsSnapshot {
        FsSnapshot {
            path: PathBuf::from(path),
            taken_at: 0,
            total_bytes,
            used_bytes,
            avail_bytes: total_bytes.saturating_sub(used_bytes),
            fs_type: "ext4".to_string(),
            btrfs: None,
        }
    }

    pub fn calls(&self) -> Vec<MeterCall> {
        lock(&self.calls).clone()
    }
}

impl SpaceMeter for FakeSpaceMeter {
    fn try_snapshot(&self, path: &Path) -> std::io::Result<FsSnapshot> {
        match *lock(&self.fail_errno) {
            Some(errno) => Err(std::io::Error::from_raw_os_error(errno)),
            None => Ok(self.snapshot(path)),
        }
    }

    fn snapshot(&self, path: &Path) -> FsSnapshot {
        lock(&self.calls).push(MeterCall::Snapshot(path.to_path_buf()));
        note(&self.log, || "snapshot".to_string());
        let mut q = lock(&self.snapshots);
        let snap = if q.len() > 1 {
            q.pop_front()
        } else {
            q.front().cloned()
        };
        snap.unwrap_or_else(|| FakeSpaceMeter::plain_snapshot(&path.to_string_lossy(), 0, 0))
    }

    fn settle(&self, path: &Path, _timeout: Duration) -> Settled {
        lock(&self.calls).push(MeterCall::Settle(path.to_path_buf()));
        note(&self.log, || "settle".to_string());
        self.settled.clone()
    }
}

// ---------------------------------------------------------------- extent probe

/// Returns the scripted attribution whatever the items are.
#[derive(Debug)]
pub struct FakeExtentProbe {
    result: crate::ports::ExtentAttribution,
}

impl FakeExtentProbe {
    pub fn new(result: crate::ports::ExtentAttribution) -> FakeExtentProbe {
        FakeExtentProbe { result }
    }
}

impl crate::ports::ExtentProbe for FakeExtentProbe {
    fn attribute(&self, _items: &[&Path], _budget: usize) -> crate::ports::ExtentAttribution {
        self.result.clone()
    }
}

// ---------------------------------------------------------------- audit sink

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditCall {
    RunStart,
    Intent,
    Result,
    RunEnd,
}

/// Records every record it is handed; `fail_nth(call, n)` makes the n-th (1-based) call of that method
/// return `Err(EIO)` without recording it.
#[derive(Debug, Default)]
#[must_use]
pub struct FakeAuditSink {
    records: Mutex<Vec<AuditRecord>>,
    seen: Mutex<Vec<AuditCall>>,
    fail: Mutex<Vec<(AuditCall, usize)>>,
    log: Mutex<Option<EventLog>>,
}

impl FakeAuditSink {
    pub fn new() -> FakeAuditSink {
        FakeAuditSink::default()
    }

    pub fn with_log(self, log: &EventLog) -> FakeAuditSink {
        *lock(&self.log) = Some(log.clone());
        self
    }

    pub fn fail_nth(self, call: AuditCall, n: usize) -> FakeAuditSink {
        lock(&self.fail).push((call, n));
        self
    }

    pub fn records(&self) -> Vec<AuditRecord> {
        lock(&self.records).clone()
    }

    /// Every call in order, failed ones included.
    pub fn calls(&self) -> Vec<AuditCall> {
        lock(&self.seen).clone()
    }

    fn take(&self, call: AuditCall, rec: AuditRecord) -> io::Result<()> {
        let n = {
            let mut seen = lock(&self.seen);
            seen.push(call);
            seen.iter().filter(|c| **c == call).count()
        };
        note(&self.log, || format!("audit:{call:?}"));
        if lock(&self.fail).contains(&(call, n)) {
            return Err(io::Error::from_raw_os_error(5));
        }
        lock(&self.records).push(rec);
        Ok(())
    }
}

impl AuditSink for FakeAuditSink {
    fn run_start(&self, rec: &RunStart) -> io::Result<()> {
        self.take(AuditCall::RunStart, AuditRecord::RunStart(rec.clone()))
    }

    fn intent(&self, rec: &Intent) -> io::Result<()> {
        self.take(AuditCall::Intent, AuditRecord::Intent(rec.clone()))
    }

    fn result(&self, rec: &ItemResult) -> io::Result<()> {
        self.take(AuditCall::Result, AuditRecord::Result(rec.clone()))
    }

    fn run_end(&self, rec: &RunEnd) -> io::Result<()> {
        self.take(AuditCall::RunEnd, AuditRecord::RunEnd(rec.clone()))
    }
}

// ---------------------------------------------------------------- run guard

/// Lock and signal fake. `interrupt(sig)` sets the abort flag and the signal, as the real one does.
#[derive(Debug, Default)]
#[must_use]
pub struct FakeRunGuard {
    abort: AbortFlag,
    signal: Mutex<Option<i32>>,
    held_by: Option<u32>,
    acquired: AtomicUsize,
    log: Mutex<Option<EventLog>>,
}

impl FakeRunGuard {
    pub fn new() -> FakeRunGuard {
        FakeRunGuard::default()
    }

    /// `acquire` fails with `FatalError::LockHeld { pid }`.
    pub fn lock_held_by(pid: u32) -> FakeRunGuard {
        FakeRunGuard {
            held_by: Some(pid),
            ..FakeRunGuard::default()
        }
    }

    pub fn with_log(self, log: &EventLog) -> FakeRunGuard {
        *lock(&self.log) = Some(log.clone());
        self
    }

    pub fn interrupt(&self, signal: i32) {
        *lock(&self.signal) = Some(signal);
        self.abort.set();
    }

    /// How many times `acquire` was called.
    pub fn acquire_calls(&self) -> usize {
        self.acquired.load(Ordering::SeqCst)
    }
}

impl RunGuard for FakeRunGuard {
    fn acquire(&self) -> Result<(), FatalError> {
        self.acquired.fetch_add(1, Ordering::SeqCst);
        note(&self.log, || "acquire".to_string());
        match self.held_by {
            Some(pid) => Err(FatalError::LockHeld { pid }),
            None => Ok(()),
        }
    }

    fn abort_flag(&self) -> &AbortFlag {
        &self.abort
    }

    fn interrupted(&self) -> Option<i32> {
        *lock(&self.signal)
    }
}

// ---------------------------------------------------------------- probe

/// Host facts from a script. Defaults: euid 1000, Linux, every process `NotRunning`, every path `Idle` and
/// not in use, no mounts. `proc_unreadable()` models a `/proc` that cannot be read: `run_state` is `Unknown`
/// and `cwd_in_use` is `(Unknown, None)` for every path.
#[derive(Debug)]
#[must_use]
pub struct FakeProbe {
    euid: u32,
    platform: Platform,
    processes: HashMap<String, Tri>,
    proc_unreadable: bool,
    run_states: HashMap<PathBuf, RunState>,
    cwd: HashMap<PathBuf, (Tri, Option<(String, u32)>)>,
    mounts: Result<Vec<MountEntry>, i32>,
}

impl Default for FakeProbe {
    fn default() -> FakeProbe {
        FakeProbe {
            euid: 1000,
            platform: Platform::Linux,
            processes: HashMap::new(),
            proc_unreadable: false,
            run_states: HashMap::new(),
            cwd: HashMap::new(),
            mounts: Ok(Vec::new()),
        }
    }
}

impl FakeProbe {
    pub fn new() -> FakeProbe {
        FakeProbe::default()
    }

    pub fn with_euid(mut self, euid: u32) -> FakeProbe {
        self.euid = euid;
        self
    }

    pub fn with_platform(mut self, p: Platform) -> FakeProbe {
        self.platform = p;
        self
    }

    pub fn process(mut self, name: &str, state: Tri) -> FakeProbe {
        self.processes.insert(name.to_string(), state);
        self
    }

    pub fn proc_unreadable(mut self) -> FakeProbe {
        self.proc_unreadable = true;
        self
    }

    pub fn run_state_of(mut self, p: impl Into<PathBuf>, s: RunState) -> FakeProbe {
        self.run_states.insert(p.into(), s);
        self
    }

    pub fn cwd_holder(mut self, p: impl Into<PathBuf>, name: &str, pid: u32) -> FakeProbe {
        self.cwd
            .insert(p.into(), (Tri::Running, Some((name.to_string(), pid))));
        self
    }

    pub fn with_mounts(mut self, m: Vec<MountEntry>) -> FakeProbe {
        self.mounts = Ok(m);
        self
    }

    /// `mounts()` fails with this raw errno.
    pub fn mounts_fail(mut self, errno: i32) -> FakeProbe {
        self.mounts = Err(errno);
        self
    }
}

impl SystemProbe for FakeProbe {
    fn euid(&self) -> u32 {
        self.euid
    }

    fn platform(&self) -> Platform {
        self.platform
    }

    fn process_named(&self, name: &str) -> Tri {
        self.processes.get(name).copied().unwrap_or(Tri::NotRunning)
    }

    fn run_state(&self, path: &Path) -> RunState {
        if self.proc_unreadable {
            return RunState::Unknown;
        }
        self.run_states.get(path).copied().unwrap_or(RunState::Idle)
    }

    fn cwd_in_use(&self, path: &Path) -> (Tri, Option<(String, u32)>) {
        if self.proc_unreadable {
            return (Tri::Unknown, None);
        }
        self.cwd
            .get(path)
            .cloned()
            .unwrap_or((Tri::NotRunning, None))
    }

    fn mounts(&self) -> io::Result<Vec<MountEntry>> {
        self.mounts.clone().map_err(io::Error::from_raw_os_error)
    }
}

// ---------------------------------------------------------------- plan store

/// Keeps the merged set in memory, through the same `merge_saved` the real store calls.
#[derive(Debug, Default)]
pub struct FakePlanStore {
    saved: Mutex<Option<SavedPlan>>,
    saves: Mutex<Vec<Vec<SavedSelection>>>,
    fail_errno: Mutex<Option<i32>>,
}

impl FakePlanStore {
    pub fn new() -> FakePlanStore {
        FakePlanStore::default()
    }

    /// `save_merged` returns `NotSaved(errno)` and keeps nothing.
    pub fn failing(errno: i32) -> FakePlanStore {
        let s = FakePlanStore::default();
        *lock(&s.fail_errno) = Some(errno);
        s
    }

    pub fn with_existing(entries: Vec<SavedSelection>) -> FakePlanStore {
        let s = FakePlanStore::default();
        *lock(&s.saved) = Some(SavedPlan { entries });
        s
    }

    /// The `new` argument of every `save_merged` call, in order.
    pub fn save_calls(&self) -> Vec<Vec<SavedSelection>> {
        lock(&self.saves).clone()
    }
}

impl PlanStore for FakePlanStore {
    fn load_last(&self) -> Option<SavedPlan> {
        lock(&self.saved).clone()
    }

    fn save_merged(&self, new: Vec<SavedSelection>, euid: u32, now: i64) -> SaveOutcome {
        lock(&self.saves).push(new.clone());
        if let Some(errno) = *lock(&self.fail_errno) {
            return SaveOutcome::NotSaved(errno);
        }
        let mut saved = lock(&self.saved);
        let existing = saved.take().map(|p| p.entries).unwrap_or_default();
        *saved = Some(SavedPlan {
            entries: merge_saved(existing, new, euid, now),
        });
        SaveOutcome::Saved
    }
}

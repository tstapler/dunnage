//! `explore --text` (Story 6.5.5, `tui/ux-explorer.md` section 8 and wireframe E8): the scan runs to
//! completion first, then ordered records, one per line, ASCII only. The model built here ([`Model`]) is the
//! one `--json` serialises, and its numbers come from the same `build_rows` the interactive view draws.
//!
//! Every line except a `Next:` line is wrapped to the width; nothing is truncated.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use disk_cleanup_core::explore::fmt::{fmt_items_cell, fmt_size_cell, share_cell};
use disk_cleanup_core::explore::hint::hint_of_node;
use disk_cleanup_core::explore::msg::{
    ClassifyFacts, Hint, NodeEvent, ScanMsg, ScanSummary, StopCause,
};
use disk_cleanup_core::explore::ports::{ScanOpts, ScanSource, TermSignals};
use disk_cleanup_core::explore::rows::{
    build_rows, Badge, Filter, Row, RowId, RowKind, RowSource, Sort,
};
use disk_cleanup_core::explore::tree::{ExploreTree, NodeId};
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::snapshot_types::FsSnapshot;
use disk_cleanup_core::SelectorId;

use crate::escape::{escape_os, shell_quote};
use crate::layout::{hard_wrap, word_wrap};
use crate::render::{errno_name, verdict_text};
use crate::view::display_path;

pub const TITLE: &str = "EXPLORE (text) - read-only. Nothing was deleted.";
pub const CLOSED: &str = "EXPLORE CLOSED - nothing was deleted.";
pub const INTERRUPTED: &str = "INTERRUPTED during scan. Nothing was deleted; no report produced.";

/// Free-text sink for the S9 progress lines (stderr when it is a TTY).
pub type Progress = Arc<dyn Fn(&str) + Send + Sync>;

/// Everything `run_text` and `run_json` read of the outside world.
pub struct ExploreEnv {
    pub scan: Arc<dyn ScanSource>,
    pub signals: Arc<dyn TermSignals>,
    /// What is displayed and scanned (never an allowed root for marking).
    pub roots: Vec<PathBuf>,
    pub scan_opts: ScanOpts,
    /// The cleaner's roots, which decide the `[build-dirs]` words.
    pub classify: RootSet,
    pub facts: ClassifyFacts,
    pub headroom: Result<FsSnapshot, String>,
    pub sort: Sort,
    /// 0 prints every entry.
    pub top: usize,
    /// 1 lists the children of each root; more also lists theirs, with the path relative to the root.
    pub depth: usize,
    pub bars: bool,
    pub width: usize,
    /// Non-ASCII names print as themselves only on a TTY.
    pub tty: bool,
    pub home: Option<PathBuf>,
    pub max_seconds: Option<u64>,
    pub progress: Option<Progress>,
    pub progress_every: Duration,
    /// Replaces the measured wall time (tests; the binary leaves it `None`).
    pub elapsed_secs: Option<f64>,
}

impl ExploreEnv {
    pub fn new(
        scan: Arc<dyn ScanSource>,
        signals: Arc<dyn TermSignals>,
        roots: Vec<PathBuf>,
        classify: RootSet,
    ) -> ExploreEnv {
        ExploreEnv {
            scan,
            signals,
            roots,
            scan_opts: ScanOpts::default(),
            classify,
            facts: ClassifyFacts::default(),
            headroom: Err("not read".to_string()),
            sort: Sort::default(),
            top: 0,
            depth: 1,
            bars: false,
            width: 80,
            tty: false,
            home: None,
            max_seconds: None,
            progress: None,
            progress_every: Duration::from_secs(2),
            elapsed_secs: None,
        }
    }
}

/// A finished (possibly partial) scan.
pub struct Scanned {
    pub tree: ExploreTree,
    pub summary: ScanSummary,
    pub errnos: HashMap<OsString, i32>,
    pub secs: f64,
}

pub enum ScanEnd {
    Done(Box<Scanned>),
    /// The exit value: 128 plus the signal.
    Interrupted(i32),
}

/// Runs the scan to its end, applying every batch to a tree; a signal ends it at once.
pub fn scan_to_completion(env: &ExploreEnv) -> ScanEnd {
    let started = Instant::now();
    let mut tree = ExploreTree::new();
    let mut opts = env.scan_opts.clone();
    opts.generation = tree.generation();
    opts.max_seconds = env.max_seconds.or(opts.max_seconds);
    let handle = env.scan.start(&env.roots, &opts);
    let mut errnos = HashMap::new();
    let mut summary = None;
    let mut last_progress = Instant::now();
    loop {
        if let Some(sig) = env.signals.interrupted() {
            handle.detach();
            return ScanEnd::Interrupted(128 + sig);
        }
        match handle.rx.recv_timeout(Duration::from_millis(20)) {
            Ok(ScanMsg::Batch {
                generation,
                mode,
                events,
            }) => {
                for e in &events {
                    if let NodeEvent::Unreadable { name, errno, .. } = e {
                        errnos.entry(name.clone()).or_insert(*errno);
                    }
                }
                let _ = tree.apply_batch(generation, mode, events);
            }
            Ok(ScanMsg::Finished(s)) => {
                if s.generation == tree.generation() {
                    tree.finish(s.truncated);
                    summary = Some(s);
                    break;
                }
            }
            Ok(ScanMsg::SourceFailed(_)) => {
                tree.finish(true);
                break;
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                tree.finish(true);
                break;
            }
        }
        if let Some(p) = &env.progress {
            if last_progress.elapsed() >= env.progress_every {
                last_progress = Instant::now();
                p(&format!(
                    "scanning: {} dirs, {:.0}s",
                    tree.directories(),
                    started.elapsed().as_secs_f64()
                ));
            }
        }
    }
    let secs = env.elapsed_secs.unwrap_or(started.elapsed().as_secs_f64());
    let summary = summary.unwrap_or_else(|| ScanSummary {
        generation: tree.generation(),
        truncated: true,
        ..ScanSummary::default()
    });
    ScanEnd::Done(Box::new(Scanned {
        tree,
        summary,
        errnos,
        secs,
    }))
}

/// How an entry's size reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    Done,
    AtLeast,
    Unreadable,
    OtherFs,
}

impl EntryState {
    pub fn json(self) -> &'static str {
        match self {
            EntryState::Done => "done",
            EntryState::AtLeast => "at_least",
            EntryState::Unreadable => "unreadable",
            EntryState::OtherFs => "other_fs",
        }
    }
}

/// One record: a row of the shared row model with the facts both outputs print.
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    /// Escaped, relative to the root, directories end with `/`; synthetic rows read `(N files)`.
    pub name: String,
    pub synthetic: Option<&'static str>,
    pub usage: u64,
    pub apparent: u64,
    pub meta: Option<u64>,
    pub items: Option<u64>,
    pub parent_usage: u64,
    pub state: EntryState,
    /// Bracketed words after the name, without the brackets.
    pub words: Vec<String>,
    pub kind: Option<String>,
    pub errors: Vec<i32>,
    pub newest_mtime: Option<i64>,
}

pub struct RootModel {
    pub path: PathBuf,
    pub usage: u64,
    pub apparent: u64,
    pub items: u64,
    pub at_least: bool,
    pub entries: Vec<Entry>,
    pub total_entries: usize,
    /// Entries folded into the last `(N smaller entries)` row by `--top`.
    pub folded: usize,
}

pub struct Model {
    pub roots: Vec<RootModel>,
    /// Cleaner kinds some displayed entry is, sorted.
    pub candidates: Vec<SelectorId>,
    /// The largest directory entry of the first root: where `Next:` points.
    pub next_dir: Option<PathBuf>,
}

struct Builder<'a> {
    env: &'a ExploreEnv,
    s: &'a Scanned,
    btrfs: bool,
    candidates: Vec<SelectorId>,
}

fn is_yes(tree: &ExploreTree, roots: &RootSet, facts: &ClassifyFacts, id: NodeId) -> bool {
    matches!(hint_of_node(tree, id, roots, facts), Hint::Yes { .. })
}

impl Builder<'_> {
    fn badge(&self, id: NodeId) -> Badge {
        hint_of_node(&self.s.tree, id, &self.env.classify, &self.env.facts).badge()
    }

    fn words_of(
        &mut self,
        row: &Row,
        node: Option<NodeId>,
    ) -> (EntryState, Vec<String>, Option<String>) {
        let tree = &self.s.tree;
        let mut words = Vec::new();
        let mut kind = None;
        let mut state = EntryState::Done;
        if row.boundary {
            state = EntryState::OtherFs;
            words.push("other filesystem: not scanned".to_string());
        } else if row.unreadable {
            state = EntryState::Unreadable;
            let why = self
                .s
                .errnos
                .get(&row.name)
                .map_or_else(|| "error".to_string(), |e| errno_name(*e));
            words.push(format!("unreadable: {why}, at least"));
        } else if row.at_least {
            state = EntryState::AtLeast;
            words.push("at least".to_string());
        }
        if let Some(id) = node {
            match hint_of_node(tree, id, &self.env.classify, &self.env.facts) {
                Hint::Yes { selector, .. } => {
                    kind = Some(selector.id().to_string());
                    self.candidates.push(selector);
                    words.push(selector.id().to_string());
                }
                Hint::Shape { selector, .. } => words.push(format!("{}?", selector.id())),
                Hint::No(_) => {
                    let (roots, facts) = (&self.env.classify, &self.env.facts);
                    let found: std::cell::Cell<Option<SelectorId>> = std::cell::Cell::new(None);
                    let n =
                        disk_cleanup_core::explore::refusal::count_items_below(tree, id, &|c| {
                            let y = is_yes(tree, roots, facts, c);
                            if y && found.get().is_none() {
                                if let Hint::Yes { selector, .. } =
                                    hint_of_node(tree, c, roots, facts)
                                {
                                    found.set(Some(selector));
                                }
                            }
                            y
                        });
                    if let (true, Some(sel)) = (n > 0, found.get()) {
                        words.push(format!("{} inside", sel.id()));
                        self.candidates.push(sel);
                    }
                }
            }
        }
        (state, words, kind)
    }

    fn entry(&mut self, row: &Row, dir: NodeId, prefix: &str, parent_usage: u64) -> Entry {
        let tree = &self.s.tree;
        let node = match &row.id {
            RowId::Dir(n) => Some(*n),
            _ => None,
        };
        let (path, name, synthetic) = match row.kind {
            RowKind::Dir | RowKind::File => {
                let esc = escape_os(&row.name, self.env.tty);
                let shown = if row.kind == RowKind::Dir {
                    format!("{prefix}{esc}/")
                } else {
                    format!("{prefix}{esc}")
                };
                (tree.path_of(dir).join(&row.name), shown, None)
            }
            RowKind::Files => (tree.path_of(dir), row.display_name(), Some("files")),
            RowKind::Smaller => (tree.path_of(dir), row.display_name(), Some("smaller")),
            RowKind::Empty => (tree.path_of(dir), row.display_name(), Some("empty")),
        };
        let (state, words, kind) = self.words_of(row, node);
        let errors = self
            .s
            .errnos
            .get(&row.name)
            .filter(|_| row.unreadable)
            .map(|e| vec![*e])
            .unwrap_or_default();
        let newest = node.map(|n| tree.newest_mtime(n)).filter(|m| *m > 0);
        Entry {
            path,
            name,
            synthetic,
            usage: row.usage,
            apparent: row.apparent,
            meta: if self.btrfs { row.meta } else { None },
            items: row.items,
            parent_usage,
            state,
            words,
            kind,
            errors,
            newest_mtime: newest,
        }
    }

    fn collect(
        &mut self,
        dir: NodeId,
        depth: usize,
        prefix: &str,
        out: &mut Vec<Entry>,
        total: &mut usize,
    ) {
        let tree = &self.s.tree;
        let parent_usage = tree.total(dir).allocated_bytes;
        let env = self.env;
        let badge = |id: NodeId| self.badge(id);
        let list = build_rows(
            &RowSource {
                tree,
                dir,
                files: None,
                badge: &badge,
            },
            env.sort,
            Filter::All,
            "",
        );
        let rows = list.rows;
        let counted = |r: &Row| match r.kind {
            RowKind::Dir | RowKind::File => 1,
            RowKind::Files | RowKind::Smaller => r.count,
            RowKind::Empty => 0,
        };
        *total += rows.iter().map(&counted).sum::<u64>() as usize;
        let keep = if env.top == 0 {
            rows.len()
        } else {
            env.top.min(rows.len())
        };
        let (shown, rest) = rows.split_at(keep);
        for r in shown {
            let e = self.entry(r, dir, prefix, parent_usage);
            let path = e.name.clone();
            out.push(e);
            if depth < env.depth {
                if let (RowKind::Dir, RowId::Dir(child)) = (r.kind, &r.id) {
                    self.collect(*child, depth + 1, &path, out, total);
                }
            }
        }
        if !rest.is_empty() {
            let n: u64 = rest.iter().map(&counted).sum();
            let sum = |f: &dyn Fn(&Row) -> u64| rest.iter().map(f).sum::<u64>();
            out.push(Entry {
                path: tree.path_of(dir),
                name: format!("({n} smaller {})", if n == 1 { "entry" } else { "entries" }),
                synthetic: Some("smaller"),
                usage: sum(&|r| r.usage),
                apparent: sum(&|r| r.apparent),
                meta: self.btrfs.then(|| sum(&|r| r.meta.unwrap_or(0))),
                items: Some(sum(&|r| r.item_count())),
                parent_usage,
                state: EntryState::Done,
                words: Vec::new(),
                kind: None,
                errors: Vec::new(),
                newest_mtime: None,
            });
        }
    }
}

/// The records of a scan, shared by the text and JSON outputs.
pub fn build_model(env: &ExploreEnv, s: &Scanned) -> Model {
    let mut b = Builder {
        env,
        s,
        btrfs: env.headroom.as_ref().is_ok_and(|h| h.btrfs.is_some()),
        candidates: Vec::new(),
    };
    let mut roots = Vec::new();
    for id in s.tree.roots() {
        let t = s.tree.total(id);
        let mut entries = Vec::new();
        let mut total = 0;
        b.collect(id, 1, "", &mut entries, &mut total);
        let folded = entries
            .iter()
            .filter(|e| e.synthetic == Some("smaller"))
            .map(|e| {
                e.name
                    .trim_start_matches('(')
                    .split(' ')
                    .next()
                    .and_then(|n| n.parse::<usize>().ok())
                    .unwrap_or(0)
            })
            .sum();
        roots.push(RootModel {
            path: s.tree.path_of(id),
            usage: t.allocated_bytes,
            apparent: t.size_bytes,
            items: t.inodes + 1,
            at_least: s.tree.is_at_least(id),
            entries,
            total_entries: total,
            folded,
        });
    }
    b.candidates.sort();
    b.candidates.dedup();
    let next_dir = roots.first().and_then(|r| {
        r.entries
            .iter()
            .find(|e| {
                e.synthetic.is_none() && e.name.ends_with('/') && e.state != EntryState::OtherFs
            })
            .map(|e| e.path.clone())
    });
    Model {
        roots,
        candidates: b.candidates,
        next_dir,
    }
}

fn wrapped(out: &mut Vec<String>, width: usize, text: &str) {
    out.extend(word_wrap(text, width));
}

fn size(n: u64) -> String {
    fmt_size_cell(n)
}

fn filesystem_block(out: &mut Vec<String>, env: &ExploreEnv) {
    let w = env.width;
    match &env.headroom {
        Err(why) => wrapped(
            out,
            w,
            &format!(
                "Filesystem state unreadable ({})",
                crate::escape::escape(why.as_bytes(), false)
            ),
        ),
        Ok(snap) => {
            let (verdict, _) = disk_cleanup_adapters::status::assess(snap);
            wrapped(
                out,
                w,
                &format!(
                    "Filesystem {}  ({})    {}",
                    display_path(snap.path.as_os_str(), env.home.as_deref(), env.tty),
                    crate::escape::escape(snap.fs_type.as_bytes(), false),
                    verdict_text(verdict)
                ),
            );
            let mut line = |t: String| {
                let body = t.trim_start();
                let indent = t.len() - body.len();
                for l in word_wrap(body, w.saturating_sub(indent)) {
                    out.push(format!("{}{l}", &t[..indent]));
                }
            };
            line(format!("  df             avail {}", size(snap.avail_bytes)));
            match &snap.btrfs {
                Some(a) => {
                    line(format!("  unallocated    {}", size(a.unallocated)));
                    line(format!(
                        "  data slack     {}   (allocated, not used)",
                        size(a.data.headroom_bytes())
                    ));
                    line(format!(
                        "  metadata slack {}",
                        size(a.metadata_headroom_bytes())
                    ));
                }
                None => line(format!(
                    "  total {}  used {}",
                    size(snap.total_bytes),
                    size(snap.used_bytes)
                )),
            }
        }
    }
}

fn fixed_cells(e: &Entry, bars: bool) -> [String; 5] {
    let share = share_cell(e.usage, e.parent_usage, 10);
    [
        size(e.usage),
        size(e.apparent),
        e.meta.map_or("n/a".into(), size),
        e.items.map_or("n/a".into(), fmt_items_cell),
        if bars { share.text() } else { share.percent },
    ]
}

const LABELS: [&str; 5] = ["USAGE", "APPARENT", "META(est)", "ITEMS", "SHARE"];

fn widths(bars: bool) -> [usize; 5] {
    [9, 9, 9, 7, if bars { 17 } else { 5 }]
}

fn join_cells(cells: &[String; 5], bars: bool) -> String {
    let w = widths(bars);
    cells
        .iter()
        .zip(w)
        .map(|(c, w)| format!("{c:>w$}"))
        .collect::<Vec<_>>()
        .join("  ")
}

fn name_with_words(e: &Entry) -> String {
    let mut s = e.name.clone();
    for w in &e.words {
        s.push_str("  [");
        s.push_str(w);
        s.push(']');
    }
    s
}

fn row_lines(out: &mut Vec<String>, width: usize, bars: bool, e: &Entry) {
    let cells = fixed_cells(e, bars);
    let fixed = join_cells(&cells, bars);
    let name = name_with_words(e);
    let single = format!("{fixed}  {name}");
    if single.chars().count() <= width {
        out.push(single);
    } else if fixed.chars().count() <= width {
        out.push(fixed);
        out.extend(
            hard_wrap(&name, width - 2)
                .into_iter()
                .map(|l| format!("  {l}")),
        );
    } else {
        for (label, cell) in LABELS.iter().zip(&cells) {
            out.push(format!("{label} {}", cell.trim()));
        }
        out.extend(
            hard_wrap(&name, width - 2)
                .into_iter()
                .map(|l| format!("  {l}")),
        );
    }
}

fn header_lines(out: &mut Vec<String>, width: usize, bars: bool) {
    let w = widths(bars);
    let head = LABELS
        .iter()
        .zip(w)
        .map(|(c, w)| format!("{c:>w$}"))
        .collect::<Vec<_>>()
        .join("  ");
    let line = format!("{head}  NAME");
    if line.chars().count() <= width {
        out.push(line);
    }
}

/// The text view. `out` gets the report; `err` the interrupted line. Returns the exit value.
pub fn run_text(env: &ExploreEnv, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let scanned = match scan_to_completion(env) {
        ScanEnd::Interrupted(code) => {
            let _ = writeln!(err, "{INTERRUPTED}");
            return code;
        }
        ScanEnd::Done(s) => *s,
    };
    let model = build_model(env, &scanned);
    let width = env.width.max(20);
    let mut lines: Vec<String> = Vec::new();
    wrapped(&mut lines, width, TITLE);
    let s = &scanned.summary;
    let cache = if s.dirs_cached > 0 {
        "cache warm"
    } else {
        "cache cold"
    };
    let roots = model
        .roots
        .iter()
        .map(|r| display_path(r.path.as_os_str(), env.home.as_deref(), env.tty))
        .collect::<Vec<_>>()
        .join(", ");
    wrapped(
        &mut lines,
        width,
        &format!(
            "Scanned {roots} in {:.1}s ({cache}, walker backend, {} dirs, {} unreadable)",
            scanned.secs,
            group(scanned.tree.directories() as u64),
            s.errors
        ),
    );
    if s.truncated {
        let why = match (s.stop, env.max_seconds) {
            (StopCause::Deadline, Some(n)) => format!("hit --max-seconds {n}"),
            _ => "stopped".to_string(),
        };
        wrapped(
            &mut lines,
            width,
            &format!("PARTIAL SCAN ({why}): sizes marked 'at least' are lower bounds"),
        );
    }
    lines.push(String::new());
    filesystem_block(&mut lines, env);
    for r in &model.roots {
        lines.push(String::new());
        wrapped(
            &mut lines,
            width,
            &format!(
                "{}  usage {}  items {}{}",
                display_path(r.path.as_os_str(), env.home.as_deref(), env.tty),
                size(r.usage),
                r.items,
                if r.at_least { "  [at least]" } else { "" }
            ),
        );
        let shown = r.total_entries - r.folded;
        wrapped(
            &mut lines,
            width,
            &format!(
                "Sorted by {}. {shown} of {} entries. Sizes marked 'at least' are lower bounds.",
                env.sort.words(),
                r.total_entries
            ),
        );
        lines.push(String::new());
        header_lines(&mut lines, width, env.bars);
        for e in &r.entries {
            row_lines(&mut lines, width, env.bars, e);
        }
    }
    lines.push(String::new());
    let mut text = lines.join("\n");
    text.push('\n');
    if let Some(dir) = &model.next_dir {
        text.push_str("Next: disk-cleanup explore ");
        text.push_str(&shell_quote(dir.as_os_str(), true));
        text.push('\n');
    }
    if !model.candidates.is_empty() {
        let kinds = model
            .candidates
            .iter()
            .map(|k| k.id())
            .collect::<Vec<_>>()
            .join(",");
        text.push_str("Next: disk-cleanup --kinds ");
        text.push_str(&kinds);
        text.push('\n');
    }
    for l in word_wrap(CLOSED, width) {
        text.push_str(&l);
        text.push('\n');
    }
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
    0
}

fn group(n: u64) -> String {
    let s = n.to_string();
    let mut o = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            o.push(',');
        }
        o.push(c);
    }
    o
}

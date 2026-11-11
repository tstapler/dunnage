//! The dispatch of `disk-cleanup` (Story 1.2.4, Task 1.2.4a): a parsed command line and a set of ports in, an
//! exit code and two streams out. Everything outside the process (the filesystem, the targets, the state
//! store, the stamp step, the apply capability) arrives through [`Ports`], so every acceptance criterion runs
//! against fakes and `main` only wires the real ones.
//!
//! The order of an apply follows `design/ux.md` 1.4 as far as this story owns it: usage checks, the stamp
//! step, then the executor (availability, plan-id gate, lock, audit, deletes). The composition order test
//! with its `Trace` is Story 2.3.4.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use disk_cleanup_adapters::state::OutcomeMode;
use disk_cleanup_adapters::status::assess;
use disk_cleanup_adapters::targets::btrfs_report::report_notes;
use disk_cleanup_adapters::targets::symlink_note::symlinked_root_notes_real;
use disk_cleanup_core::executor::{Mode, Refused, RunMeta, RunOptions, RunReport};
use disk_cleanup_core::plan::{GateRenderCtx, PlanId, Planned};
use disk_cleanup_core::ports::Env;
use disk_cleanup_core::selector::{KindSelector, Selection};
use disk_cleanup_core::target::{Target, NOT_IN_BUILD};
use disk_cleanup_core::{
    ApplyToken, BuildInfo, DiskVerdict, RunRoots, SaveOutcome, SelectorId, Warning,
};

use crate::apply::{finished_line, render_apply, render_refused, ApplyInput, LastDryRun};
use crate::args::{AuditCommand, Cli, Command, SortArg, SteamCommand};
use crate::render::{render_plan, wrapper_note};
use crate::render_json::{
    apply_doc, fatal_doc, gate_refusal_doc, plan_doc, to_text, SCHEMA_VERSION,
};
use crate::terminal::Terminal;
use crate::view::{
    saved_entries, ErrorEntry, FsBlock, Options, PlanInput, PlanView, ScanFacts, SortKey,
};
use crate::wiring::audit::{GateExit, StampOutcome};
use crate::{audit_cmd, kinds, status};

/// The refusal of a build that ships no apply path (Gate A; the token is minted by Story 2.1.6).
pub const APPLY_NOT_ENABLED: &str = "error: apply is not enabled in this build (kinds register per gate; see 'disk-cleanup kinds'). Nothing was deleted.";

/// Whether this binary can delete at all.
pub enum ApplyMode<'a> {
    Disabled,
    /// Mints the capability for one run. The production mint site is `wiring::token` (Story 2.1.6).
    Granted(&'a dyn Fn() -> ApplyToken),
}

/// The effect of writing the one `outcome.jsonl` line.
pub type OutcomeFn<'a> = dyn Fn(OutcomeMode, DiskVerdict, u64, Option<u64>) + 'a;

/// What a run is wired to.
pub struct Ports<'a> {
    pub env: &'a Env<'a>,
    pub targets: Vec<&'a dyn Target>,
    pub apply: ApplyMode<'a>,
    /// The stamp step of an apply, given the gate the selection needs; `StampOutcome::Confined` skips it.
    pub stamp: &'a dyn Fn(char) -> Result<StampOutcome, GateExit>,
    pub build: BuildInfo,
    pub roots: RunRoots,
    pub scan_roots: usize,
    pub exe: PathBuf,
    pub term: Terminal,
    pub stdout_tty: bool,
    pub run_id: String,
    /// `DISK_CLEANUP_VIA`.
    pub via: Option<String>,
    pub outcome: &'a OutcomeFn<'a>,
    /// `note: created 2 reserve files ...`, printed on stderr before the run.
    pub reserves_note: Option<String>,
    /// `roots` of the owner's `config.toml`, for `audit verify`.
    pub config_roots: Option<Vec<PathBuf>>,
}

pub struct Io<'a> {
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
}

impl Io<'_> {
    /// A closed pipe (`| head`) is not an error of the run (UX-38).
    fn stdout(&mut self, text: &str) {
        let _ = self.out.write_all(text.as_bytes());
        let _ = self.out.flush();
    }

    fn stderr(&mut self, text: &str) {
        let _ = self.err.write_all(text.as_bytes());
        let _ = self.err.flush();
    }

    fn stderr_line(&mut self, text: &str) {
        self.stderr(&format!("{text}\n"));
    }
}

impl Ports<'_> {
    fn registered(&self) -> Vec<SelectorId> {
        let mut v: Vec<SelectorId> = self
            .targets
            .iter()
            .flat_map(|t| t.selectors().iter().copied())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    fn available_label(&self, s: SelectorId) -> String {
        match self.targets.iter().find(|t| t.selectors().contains(&s)) {
            Some(t) => t.gate(self.env).available_label(self.env.probe.platform()),
            None => NOT_IN_BUILD.to_string(),
        }
    }
}

/// Runs a parsed command line and returns the exit code.
pub fn run(cli: &Cli, p: &Ports, io: &mut Io) -> i32 {
    match &cli.command {
        Some(Command::Status(args)) => {
            let mut s = status::run(args, cli.json, p.env.space, &p.term, Some(&p.env.dirs.home));
            if !cli.json && s.outcome.is_some() {
                for n in report_notes(p.env, &args.path) {
                    s.stdout.push_str("Note: ");
                    s.stdout.push_str(&n);
                    s.stdout.push('\n');
                }
            }
            if let Some((verdict, free, unalloc)) = s.outcome {
                (p.outcome)(OutcomeMode::Status, verdict, free, unalloc);
            }
            io.stdout(&s.stdout);
            io.stderr(&s.stderr);
            s.code
        }
        Some(Command::Kinds) => {
            io.stdout(&kinds::run(&|s| p.available_label(s), cli.json, &p.term));
            0
        }
        Some(Command::Steam {
            command: SteamCommand::List,
        }) => {
            io.stderr_line(
                "error: 'steam list' is not available in this build (see 'disk-cleanup kinds')",
            );
            64
        }
        Some(Command::Audit {
            command: AuditCommand::Verify { log },
        }) => {
            let log = log
                .clone()
                .unwrap_or_else(|| p.env.dirs.state.join("audit.jsonl"));
            let a = audit_cmd::run(&log, &p.env.dirs.home, p.config_roots.as_deref(), cli.json);
            io.stdout(&a.stdout);
            io.stderr(&a.stderr);
            a.code
        }
        Some(Command::Scan(args)) => crate::scan_cmd::run_process(args, cli.json, io),
        None => cleaner(cli, p, io),
    }
}

fn cleaner(cli: &Cli, p: &Ports, io: &mut Io) -> i32 {
    let registered = p.registered();
    let sel = match cli.selection(&registered, &KindSelector::default_apply()) {
        Ok(s) => s,
        Err(e) => {
            io.stderr_line(&e.0);
            return 64;
        }
    };
    if let Some(note) = &p.reserves_note {
        io.stderr_line(note);
    }
    if cli.apply {
        apply_mode(cli, &sel, p, io)
    } else {
        plan_mode(cli, &sel, &registered, p, io)
    }
}

fn run_meta(p: &Ports, stamp: &StampOutcome) -> RunMeta {
    let (stamp_summary, break_glass) = match stamp {
        StampOutcome::Confined => (None, None),
        StampOutcome::Verified(s) => (Some(s.clone()), None),
        StampOutcome::BrokeGlass { break_glass, .. } => (None, Some(break_glass.clone())),
    };
    RunMeta {
        run_id: p.run_id.clone(),
        build: p.build.clone(),
        stamp: stamp_summary,
        break_glass,
        roots: p.roots.clone(),
        verdict_start: None,
    }
}

fn options(cli: &Cli, p: &Ports, stamp: &StampOutcome) -> RunOptions {
    RunOptions {
        jobs: cli.jobs.max(1),
        settle_timeout: Duration::from_secs(30),
        meter_path: None,
        meta: run_meta(p, stamp),
    }
}

fn sort_key(s: SortArg) -> SortKey {
    match s {
        SortArg::Size => SortKey::Size,
        SortArg::Age => SortKey::Age,
        SortArg::Kind => SortKey::Kind,
    }
}

fn unbuilt(registered: &[SelectorId]) -> Vec<String> {
    SelectorId::ALL
        .iter()
        .filter(|s| !registered.contains(s))
        .map(|s| s.id().to_string())
        .collect()
}

fn fs_block(report: &RunReport) -> FsBlock {
    let snapshot = report.summary.fs_before.clone();
    let (verdict, _) = assess(&snapshot);
    FsBlock { snapshot, verdict }
}

fn plan_mode(cli: &Cli, sel: &Selection, registered: &[SelectorId], p: &Ports, io: &mut Io) -> i32 {
    let started = std::time::Instant::now();
    let executor = disk_cleanup_core::executor::Executor::new(
        p.env,
        p.targets.clone(),
        options(cli, p, &StampOutcome::Confined),
    );
    let report = match executor.run(sel, &Mode::Plan, None) {
        Ok(r) => r,
        Err(r) => return refused(cli, &r, sel, p, io),
    };
    if let Some(sig) = p.env.guard.interrupted() {
        io.stderr_line("INTERRUPTED during scan. Nothing was deleted; no report produced.");
        return 128 + sig;
    }
    let fs = fs_block(&report);
    let euid = p.env.probe.euid();
    let mut input = PlanInput {
        plan: &report.plan,
        selection: sel,
        saved: SaveOutcome::Saved,
        scan: ScanFacts {
            roots: p.scan_roots,
            secs: started.elapsed().as_secs_f64(),
            cache_warm: false,
            backend: "walker".into(),
            dirs: 0,
            unreadable: 0,
        },
        fs: Some(fs.clone()),
        partial_scan: None,
        not_built: unbuilt(registered),
        notes: [
            report.notes.clone(),
            symlinked_root_notes_real(p.env.fs, &p.env.dirs.home, &report.plan.items),
            report_notes(p.env, &p.env.dirs.home),
        ]
        .concat(),
        wrapper_note: wrapper_note(p.via.as_deref(), cli.dry_run),
        exe: p.exe.clone(),
        euid,
        state_dir: p.env.dirs.state.clone(),
        home: Some(p.env.dirs.home.clone()),
        facts: HashMap::new(),
        opts: Options {
            top: cli.top,
            sort: sort_key(cli.sort),
            verbose: cli.verbose,
            quiet: cli.quiet,
        },
        errors: Vec::<ErrorEntry>::new(),
        tty: p.stdout_tty,
    };
    let entries = saved_entries(&input);
    input.saved = p.env.plans.save_merged(entries, euid, report.plan.now);
    if let SaveOutcome::NotSaved(errno) = input.saved {
        io.stderr_line(
            &Warning::PlanNotSaved {
                errno,
                path: p.env.dirs.state.join("last_plan"),
            }
            .text(),
        );
    }
    let view = PlanView::build(&input);
    let text = if cli.json {
        to_text(&plan_doc(&input, &view, 0))
    } else {
        render_plan(&input, &view, &p.term)
    };
    io.stdout(&text);
    (p.outcome)(
        OutcomeMode::Plan,
        fs.verdict,
        fs.snapshot.avail_bytes,
        fs.snapshot.btrfs.map(|b| b.unallocated),
    );
    0
}

fn apply_mode(cli: &Cli, sel: &Selection, p: &Ports, io: &mut Io) -> i32 {
    let ApplyMode::Granted(mint) = &p.apply else {
        io.stderr_line(APPLY_NOT_ENABLED);
        return 64;
    };
    let supplied = cli.plan_id.map(disk_cleanup_core::plan::PlanIdArg::plan_id);
    let kinds: Vec<disk_cleanup_core::Kind> = sel
        .kinds()
        .ids()
        .flat_map(|s| s.kinds().iter().copied())
        .collect();
    let stamp = match (p.stamp)(disk_cleanup_core::required_gate(&kinds)) {
        Ok(s) => s,
        Err(e) => return stamp_refusal(cli, &e, supplied, io),
    };
    let saved_before = p.env.plans.load_last();
    let executor = disk_cleanup_core::executor::Executor::new(
        p.env,
        p.targets.clone(),
        options(cli, p, &stamp),
    );
    let mut report = match executor.run(sel, &Mode::Apply(mint()), supplied) {
        Ok(r) => r,
        Err(r) => return refused(cli, &r, sel, p, io),
    };
    report
        .summary
        .notes
        .extend(report_notes(p.env, &p.env.dirs.home));
    let warning = match &stamp {
        StampOutcome::BrokeGlass { warning, .. } => Some(warning.clone()),
        _ => None,
    };
    let exit = report.exit.code();
    let will: Vec<_> = report
        .plan
        .items
        .iter()
        .filter(|i| i.planned == Planned::Will)
        .collect();
    let last_dry_run = saved_before.as_ref().map(|s| {
        let id = report.plan_id.to_string();
        if s.entries.iter().any(|e| e.plan_id == id) {
            LastDryRun::Matches
        } else {
            LastDryRun::Differs(
                s.entries
                    .last()
                    .map_or_else(String::new, |e| e.plan_id.clone()),
            )
        }
    });
    let not_attempted = report.abort.map(|_| {
        report
            .summary
            .outcomes
            .iter()
            .filter(|o| {
                matches!(
                    o.status,
                    disk_cleanup_core::Status::Skipped(disk_cleanup_core::SkipReason::Aborted)
                )
            })
            .count()
    });
    let input = ApplyInput {
        plan_id: report.plan_id,
        selection: sel,
        gate_bypass: warning,
        audit_path: Some(p.env.dirs.state.join("audit.jsonl")),
        selected_items: will.len(),
        selected_est: will.iter().map(|i| i.candidate.est_bytes().0).sum(),
        last_dry_run,
        summary: &report.summary,
        exit_code: exit,
        not_attempted,
        exe: p.exe.clone(),
        home: Some(p.env.dirs.home.clone()),
        labels: HashMap::new(),
        facts: HashMap::new(),
        verbose: cli.verbose,
        quiet: cli.quiet,
        tty: p.stdout_tty,
    };
    for n in &report.notes {
        io.stderr_line(n);
    }
    if let Some(f) = &report.fatal {
        io.stderr_line(&f.text());
    }
    let text = if cli.json {
        to_text(&apply_doc(&input))
    } else {
        render_apply(&input, &p.term)
    };
    io.stdout(&text);
    let verdict = report.summary.verdict_after;
    let after = report
        .summary
        .fs_after
        .as_ref()
        .unwrap_or(&report.summary.fs_before);
    (p.outcome)(
        OutcomeMode::Apply,
        verdict,
        after.avail_bytes,
        after.btrfs.map(|b| b.unallocated),
    );
    exit
}

fn error_doc(code: &str, message: &str, plan_id: Option<PlanId>, exit: i32) -> String {
    to_text(&serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "mode": "apply",
        "exit_code": exit,
        "plan_id": plan_id.map(|i| i.to_string()),
        "items": [],
        "errors": [{"code": code, "path": null, "message": message}],
    }))
}

fn stamp_refusal(cli: &Cli, e: &GateExit, supplied: Option<PlanId>, io: &mut Io) -> i32 {
    io.stderr_line(&e.text);
    if cli.json {
        io.stdout(&error_doc(e.json_code, &e.text, supplied, 64));
    }
    64
}

/// A run that ended before any item was touched.
fn refused(cli: &Cli, r: &Refused, sel: &Selection, p: &Ports, io: &mut Io) -> i32 {
    for n in &r.notes {
        if !r.text.contains(n.as_str()) {
            io.stderr_line(n);
        }
    }
    io.stderr_line(&r.text);
    let code = r.exit.code();
    if let Some(refusal) = &r.refusal {
        if cli.json {
            let saved = p.env.plans.load_last();
            let ctx = GateRenderCtx {
                selection: sel,
                saved: saved.as_ref(),
                euid: p.env.probe.euid(),
                state_dir_low: None,
                unavailable: Vec::new(),
                home: Some(p.env.dirs.home.as_path()),
            };
            io.stdout(&to_text(&gate_refusal_doc(refusal, &ctx)));
        }
        return code;
    }
    let Some(plan_id) = r.plan_id else {
        // Every selected kind is unavailable: nothing ran, so there is no marker either.
        return code;
    };
    if cli.json {
        let doc = match &r.fatal {
            Some(f) => fatal_doc("apply", Some(&plan_id.to_string()), f),
            None => serde_json::from_str(&error_doc("refused", &r.text, Some(plan_id), code))
                .unwrap_or_default(),
        };
        io.stdout(&to_text(&doc));
    } else {
        io.stdout(&render_refused(plan_id, None, code, &p.term));
    }
    code
}

/// The closing line a refused run prints, for tests.
pub fn closing_line(exit: i32) -> String {
    finished_line(exit)
}

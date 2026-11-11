//! disk-cleanup: reclaim disk space. Dry run by default; nothing is deleted without `--apply`.
//!
//! This file only wires the real ports and exits with the code `app::run` returns: parsing is
//! `disk_cleanup_cli::args`, the commands are `app`, `status`, `kinds` and `audit_cmd`, and every decision
//! lives in `disk_cleanup_core` so it is tested without a filesystem.

#![forbid(unsafe_code)]

mod version;

use std::ffi::OsString;
use std::io::IsTerminal;

use disk_cleanup_adapters::config::{config_path, read_config};
use disk_cleanup_adapters::signals::SwallowBrokenPipe;
use disk_cleanup_adapters::state::OutcomeMode;
use disk_cleanup_adapters::steam::{default_steam_roots, library_roots};
use disk_cleanup_cli::app::{self, ApplyMode, Io, Ports};
use disk_cleanup_cli::args::{self, Cli, Command, Parsed};
use disk_cleanup_cli::render_json::{fatal_doc, to_text};
use disk_cleanup_cli::terminal;
use disk_cleanup_cli::wiring::audit::{build_info, StampOutcome};
use disk_cleanup_cli::wiring::compose;
use disk_cleanup_cli::wiring::confine;
use disk_cleanup_cli::wiring::dirs::{real_canon, resolve_real_dirs};
use disk_cleanup_cli::wiring::env::{read_root, Wired};
use disk_cleanup_cli::wiring::kinds::{KindCtx, KindRegistry};
use disk_cleanup_cli::wiring::run_guard;
use disk_cleanup_core::roots::{RootError, RootSet};
use disk_cleanup_core::{Dirs, DiskVerdict, FatalError};

const EXIT_USAGE: i32 = 64;

fn main() {
    let code = real_main();
    std::process::exit(code);
}

fn real_main() -> i32 {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if version::requested(args.iter().cloned()) {
        println!("{}", version::line());
        return 0;
    }
    let cli = match args::parse(args) {
        Parsed::Run(cli) => cli,
        Parsed::Exit {
            code,
            stdout,
            stderr,
        } => {
            print_raw(&stdout, &stderr);
            return code;
        }
    };
    // Read-only and stateless: dispatched before any Env, lock or audit sink exists (Story 1.2.5).
    if let Some(Command::Scan(scan)) = &cli.command {
        return disk_cleanup_cli::scan_cmd::run_wired(scan, cli.json);
    }
    if let Some(msg) = confine::startup_refusal(&confine::current_vars(), cli.apply) {
        eprintln!("{msg}");
        return EXIT_USAGE;
    }
    let base = match resolve_real_dirs() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}", e.text());
            return e.exit_code();
        }
    };
    let roots = match resolve_roots(&cli, &base) {
        Ok(r) => r,
        Err(code) => return code,
    };
    let writes_state = match &cli.command {
        None => !cli.apply,
        Some(Command::Status(_)) => true,
        Some(_) => false,
    };
    // The explorer has its own composition root and takes no lock, audit sink or outcome line (Story 6.5.4).
    if let Some(Command::Explore(args)) = &cli.command {
        return disk_cleanup_cli::explore::run::run_wired(&cli, args, &base, &roots);
    }
    if compose::is_cleaner_apply(&cli) {
        let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
        return compose::run_apply(argv, &cli, &base, &roots);
    }
    run_wired(&cli, &base, &roots, writes_state)
}

fn print_raw(stdout: &str, stderr: &str) {
    use std::io::Write;
    let _ = std::io::stdout().write_all(stdout.as_bytes());
    let _ = std::io::stderr().write_all(stderr.as_bytes());
}

/// The scan roots of the run. Only the cleaner needs them to be valid; the other commands carry the
/// defaults.
fn resolve_roots(cli: &Cli, base: &Dirs) -> Result<RootSet, i32> {
    let cleaner = cli.command.is_none() || matches!(cli.command, Some(Command::Explore(_)));
    let config = match read_config(&config_path(base)) {
        Ok(c) => c,
        Err(e) if cleaner => {
            eprintln!("error: {e}. Nothing was changed. Fix or remove the file, then run again.");
            return Err(EXIT_USAGE);
        }
        Err(_) => disk_cleanup_adapters::config::Config::default(),
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| base.home.clone());
    // The global --root widens the cleaner's roots only; explore never does (ADR-006 O-8: config.toml is the one place).
    let flag_roots = if cli.command.is_none() {
        cli.root.as_slice()
    } else {
        &[]
    };
    match RootSet::resolve_on(
        base,
        flag_roots,
        config.roots.as_deref(),
        &cwd,
        &real_canon,
        &|p| disk_cleanup_core::ports::Fs::fs_type(&disk_cleanup_cli::wiring::env::RealFs, p),
    ) {
        Ok(r) if cleaner => Ok(with_steam_libraries(r, base)),
        Ok(r) => Ok(r),
        Err(e) if cleaner => Err(root_failure(cli, &e)),
        Err(_) => Err(EXIT_USAGE),
    }
}

/// A build that registers `steam` (Gate D) bounds its deletes by the libraries Steam lists itself. No flag
/// or config value can add one.
fn with_steam_libraries(roots: RootSet, base: &Dirs) -> RootSet {
    if disk_cleanup_cli::wiring::kinds::build_gate() < 'D' {
        return roots;
    }
    let platform =
        disk_cleanup_core::ports::SystemProbe::platform(&disk_cleanup_adapters::RealProbe);
    let libraries = default_steam_roots(&base.home, platform)
        .first()
        .map(|root| library_roots(root))
        .unwrap_or_default();
    roots.with_steam_libraries(libraries)
}

fn root_failure(cli: &Cli, e: &RootError) -> i32 {
    eprintln!("{}", e.text());
    if let RootError::Unreadable { path, errno } = e {
        if cli.json {
            let fatal = FatalError::RootsUnreadable {
                path: path.clone(),
                errno: *errno,
            };
            let mode = if cli.apply { "apply" } else { "plan" };
            print!("{}", to_text(&fatal_doc(mode, None, &fatal)));
        }
    }
    e.exit_code()
}

fn run_wired(cli: &Cli, base: &Dirs, roots: &RootSet, writes_state: bool) -> i32 {
    let wired = Wired::build(base, writes_state);
    let env = wired.env();
    let read = read_root();
    let ops = {
        let read = read.clone();
        move || disk_cleanup_adapters::RealDirOps::confined_to(&read)
    };
    let registry = KindRegistry::build(&KindCtx {
        roots: roots.clone(),
        fs_root: read.clone(),
        ops: &ops,
        fixture_mode: std::env::var_os("DC_TEST_ROOT").is_some(),
    });
    let capable = registry.apply_capable();
    if cli.command.is_none() {
        if let Err(e) = run_guard::install_plan() {
            eprintln!(
                "warning: cannot install the signal handlers ({e}); Ctrl-C ends the run at once."
            );
        }
    }

    let build = build_info();
    let run_id = format!(
        "{:x}-{:x}",
        disk_cleanup_core::ports::Clock::now_epoch(&wired.clock),
        std::process::id()
    );
    let via = std::env::var("DISK_CLEANUP_VIA").ok();
    let outcome_run_id = run_id.clone();
    let outcome_via = via.clone();
    let outcome = |mode: OutcomeMode, verdict: DiskVerdict, free: u64, unalloc: Option<u64>| {
        if let Err(e) = wired.record_outcome(
            mode,
            &outcome_run_id,
            verdict,
            free,
            unalloc,
            outcome_via.as_deref(),
        ) {
            eprintln!("warning: could not record the run in outcome.jsonl ({e}).");
        }
    };
    // Plan, status and the read-only commands never reach the stamp step; `--apply` is `wiring::compose`.
    let stamp = |_gate: char| Ok(StampOutcome::Confined);
    let reserves_note = wired
        .state
        .dir
        .as_ref()
        .filter(|_| wired.state.reserves_created)
        .map(|d| {
            disk_cleanup_core::Warning::ReservesCreated {
                state_dir: d.paths.dir.clone(),
            }
            .text()
        });
    let ports = Ports {
        env: &env,
        targets: registry.targets(),
        apply: ApplyMode::Disabled,
        apply_capable: Some(&capable),
        stamp: &stamp,
        build: build.clone(),
        roots: roots.run_roots(),
        scan_roots: roots.scan_roots().len(),
        exe: std::env::current_exe().unwrap_or_default(),
        term: terminal::detect(cli.color_choice()),
        stdout_tty: std::io::stdout().is_terminal(),
        run_id,
        via,
        outcome: &outcome,
        reserves_note,
        config_roots: read_config(&config_path(base)).ok().and_then(|c| c.roots),
    };
    // Unlocked handles: the signal watcher writes to stderr while this thread works.
    let mut out = SwallowBrokenPipe::new(std::io::stdout());
    let mut err = std::io::stderr();
    app::run(
        cli,
        &ports,
        &mut Io {
            out: &mut out,
            err: &mut err,
        },
    )
}

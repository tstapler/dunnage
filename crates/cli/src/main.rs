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
use disk_cleanup_adapters::state::OutcomeMode;
use disk_cleanup_cli::app::{self, ApplyMode, Io, Ports};
use disk_cleanup_cli::args::{self, Cli, Command, Parsed};
use disk_cleanup_cli::render_json::{fatal_doc, to_text};
use disk_cleanup_cli::terminal;
use disk_cleanup_cli::wiring::audit::{build_info, real_stamp_gate};
use disk_cleanup_cli::wiring::cache_target::CacheTarget;
use disk_cleanup_cli::wiring::confine;
use disk_cleanup_cli::wiring::dirs::{real_canon, resolve_real_dirs};
use disk_cleanup_cli::wiring::env::{read_root, Wired};
use disk_cleanup_core::roots::{RootError, RootSet};
use disk_cleanup_core::target::Target;
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
    let cleaner = cli.command.is_none();
    let config = match read_config(&config_path(base)) {
        Ok(c) => c,
        Err(e) if cleaner => {
            eprintln!("error: {e}. Nothing was changed. Fix or remove the file, then run again.");
            return Err(EXIT_USAGE);
        }
        Err(_) => disk_cleanup_adapters::config::Config::default(),
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| base.home.clone());
    let flag_roots = if cleaner { cli.root.as_slice() } else { &[] };
    match RootSet::resolve(base, flag_roots, config.roots.as_deref(), &cwd, &real_canon) {
        Ok(r) => Ok(r),
        Err(e) if cleaner => Err(root_failure(cli, &e)),
        Err(_) => Err(EXIT_USAGE),
    }
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
    let cache = CacheTarget::new(roots.clone(), read.clone(), read);
    let targets: Vec<&dyn Target> = vec![&cache];

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
    let ungated = std::env::var("DISK_CLEANUP_UNGATED").ok();
    let confined = std::env::var_os("DC_TEST_ROOT").is_some();
    let state_dir = wired.dirs.state.clone();
    let stamp =
        |gate: char| real_stamp_gate(&state_dir, &build, gate, confined, ungated.as_deref());
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
        targets,
        apply: ApplyMode::Disabled,
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
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    app::run(
        cli,
        &ports,
        &mut Io {
            out: &mut out,
            err: &mut err,
        },
    )
}

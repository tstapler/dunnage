//! The `explore` entry point (Story 6.5.4): path checks, the mode decision and the three views. The
//! composition of the real ports is `wiring::explore`; the views are `text`, `json` and the `tui` crate.
//!
//! Exit codes (UX-T31): 0 for a view that completed (also with unreadable paths or a partial scan), 2 for a
//! PATH that is missing or unreadable, 64 for usage, 74 for a terminal that cannot be set up, 130 for SIGINT
//! and 143 for SIGTERM, SIGHUP and SIGQUIT. 1, 3 and 4 are never returned.

use std::path::PathBuf;

use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::{Dirs, FatalError};

pub const EXIT_FATAL: i32 = 2;
pub const EXIT_TERMINAL: i32 = 74;
pub const EXIT_SIGINT: i32 = 130;
pub const EXIT_SIGTERM: i32 = 143;

/// `128 + signal` from the scan loop, folded into the table: SIGHUP (129) and SIGQUIT (131) end as 143.
pub fn normalize_exit(code: i32) -> i32 {
    match code {
        129 | 131 | 143 => EXIT_SIGTERM,
        other => other,
    }
}

/// The directories to show, absolute and canonical (the form the cleaner's roots have). A missing,
/// unreadable or non-directory path is the `RootsUnreadable` fatal error (exit 2).
pub fn resolve_display_roots(requested: &[PathBuf]) -> Result<Vec<PathBuf>, FatalError> {
    let mut out: Vec<PathBuf> = Vec::new();
    for path in requested {
        let canonical = std::fs::canonicalize(path).map_err(|e| FatalError::RootsUnreadable {
            path: path.clone(),
            errno: e.raw_os_error().unwrap_or(2),
        })?;
        if !canonical.is_dir() {
            return Err(FatalError::RootsUnreadable {
                path: path.clone(),
                errno: 20,
            });
        }
        if !out.contains(&canonical) {
            out.push(canonical);
        }
    }
    Ok(out)
}

#[cfg(not(feature = "explore"))]
pub fn run_wired(
    _cli: &crate::args::Cli,
    _args: &crate::args::ExploreArgs,
    _base: &Dirs,
    _roots: &RootSet,
) -> i32 {
    eprintln!("error: 'explore' is not available in this build. Nothing was deleted.");
    64
}

#[cfg(feature = "explore")]
pub use interactive::run_wired;

#[cfg(feature = "explore")]
mod interactive {
    use std::io::{IsTerminal, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::sync::Arc;

    use disk_cleanup_adapters::explore::cache_paint::CacheMode;
    use disk_cleanup_adapters::signals::SwallowBrokenPipe;
    use disk_cleanup_adapters::RealTermSignals;
    use disk_cleanup_core::explore::ports::TermSignals;
    use disk_cleanup_core::explore::rows::{Sort, SortKey};
    use disk_cleanup_tui::app::{ExplorerConfig, Ports};
    use disk_cleanup_tui::terminal::{query_size, run_on_stdout, write_stderr_lines};

    use super::{normalize_exit, resolve_display_roots, Dirs, RootSet, EXIT_FATAL, EXIT_TERMINAL};
    use crate::args::{Cli, ExploreArgs, ExploreSortArg};
    use crate::explore::json::run_json;
    use crate::explore::mode::{choose_mode, Mode, ModeFlags};
    use crate::explore::text::{run_text, Progress};
    use crate::terminal;
    use crate::wiring::explore::{build_explorer_env, build_text_env, default_jobs, Settings};

    fn sort_of(arg: ExploreSortArg) -> Sort {
        Sort::new(match arg {
            ExploreSortArg::Usage => SortKey::Usage,
            ExploreSortArg::Apparent => SortKey::Apparent,
            ExploreSortArg::Meta => SortKey::Meta,
            ExploreSortArg::Items => SortKey::Items,
            ExploreSortArg::Name => SortKey::Name,
        })
    }

    fn cache_mode(args: &ExploreArgs) -> CacheMode {
        if args.no_cache {
            CacheMode::Off
        } else if args.rescan {
            CacheMode::Rescan
        } else {
            CacheMode::Use
        }
    }

    pub fn run_wired(cli: &Cli, args: &ExploreArgs, base: &Dirs, roots: &RootSet) -> i32 {
        let display = match resolve_display_roots(&args.display_roots()) {
            Ok(d) => d,
            Err(fatal) => {
                eprintln!("{}", fatal.text());
                return fatal.exit_code();
            }
        };
        let term = std::env::var("TERM").ok();
        let size = query_size();
        let mode = choose_mode(
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
            term.as_deref(),
            size,
            ModeFlags {
                text: args.wants_text(),
                json: cli.json,
            },
            std::env::var_os("DISK_CLEANUP_PLAIN").is_some_and(|v| !v.is_empty()),
        );
        let signals = match RealTermSignals::new() {
            Ok(s) => Arc::new(s) as Arc<dyn TermSignals>,
            Err(e) => {
                eprintln!("error: cannot install the signal handlers ({e}). Nothing was deleted.");
                return EXIT_FATAL;
            }
        };
        let screen = terminal::detect(cli.color_choice());
        let settings = Settings {
            display_roots: display,
            jobs: args.jobs.unwrap_or_else(default_jobs),
            sort: sort_of(args.sort),
            cache: cache_mode(args),
            max_seconds: args.max_seconds,
            tty: screen.tty,
        };
        match mode {
            Mode::Interactive => interactive(base, roots, &settings, signals, size, &screen),
            Mode::Text { note } => {
                if let Some(n) = note {
                    eprintln!("{n}");
                }
                batch(args, base, roots, &settings, signals, &screen, false)
            }
            Mode::Json => batch(args, base, roots, &settings, signals, &screen, true),
        }
    }

    /// `--text`, `--json` and the automatic fallback: scan to the end, print once.
    fn batch(
        args: &ExploreArgs,
        base: &Dirs,
        roots: &RootSet,
        settings: &Settings,
        signals: Arc<dyn TermSignals>,
        screen: &terminal::Terminal,
        json: bool,
    ) -> i32 {
        let mut env = build_text_env(base, roots, settings, signals);
        env.top = args.top;
        env.depth = args.depth.max(1);
        env.bars = args.bars;
        env.width = screen.width;
        if std::io::stderr().is_terminal() && !json {
            let progress: Progress = Arc::new(|line| {
                let _ = writeln!(std::io::stderr(), "{line}");
            });
            env.progress = Some(progress);
        }
        let mut out = SwallowBrokenPipe::new(std::io::stdout());
        let mut err = std::io::stderr();
        let code = if json {
            run_json(&env, &mut out, &mut err)
        } else {
            run_text(&env, &mut out, &mut err)
        };
        normalize_exit(code)
    }

    fn interactive(
        base: &Dirs,
        roots: &RootSet,
        settings: &Settings,
        signals: Arc<dyn TermSignals>,
        size: Option<(u16, u16)>,
        screen: &terminal::Terminal,
    ) -> i32 {
        let Some(size) = size else {
            eprintln!("error: the terminal size cannot be read. Use --text. Nothing was deleted.");
            return EXIT_TERMINAL;
        };
        let env = build_explorer_env(base, roots, settings);
        // Before the alternate screen is entered, so it is still on the scrollback afterwards.
        if let Some(note) = &env.reserves_note {
            eprintln!("{note}");
        }
        let mut cfg = ExplorerConfig::new(settings.display_roots.clone(), roots.clone());
        cfg.scan_opts.clone_from(&env.scan_opts);
        cfg.sort = settings.sort;
        cfg.cleaner_roots.clone_from(&env.cleaner_roots);
        cfg.config_path.clone_from(&env.config_path);
        cfg.home = Some(base.home.clone());
        cfg.tty = true;
        cfg.color = screen.color;
        cfg.mark_check.clone_from(&env.mark_check);
        cfg.owner_of = Some(env.owner_of.clone());
        cfg.device_label = Some(env.device_label.clone());
        cfg.now_unix = env.now_unix;
        cfg.cache_age.clone_from(&env.cache_age);
        cfg.initial_headroom.clone_from(&env.initial_headroom);
        cfg.headroom_path.clone_from(&env.headroom_path);
        cfg.start_note.clone_from(&env.start_note);
        let ports = Ports {
            scan: env.scan.clone(),
            lister: env.lister.clone(),
            signals,
            actions: env.actions.clone(),
        };
        match run_on_stdout(ports, cfg, size) {
            Err(e) => {
                eprintln!(
                    "error: cannot set up the terminal ({e}). Use --text. Nothing was deleted."
                );
                EXIT_TERMINAL
            }
            Ok(exit) => {
                let mut out = std::io::stdout().lock();
                for line in &exit.stdout_lines {
                    let _ = out.write_all(line.as_bytes());
                    let _ = out.write_all(b"\n");
                }
                let _ = out.flush();
                write_stderr_lines(&exit.stderr_lines);
                exit.code
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn signal_exits_should_fold_into_the_documented_table() {
        assert_eq!(normalize_exit(130), 130);
        assert_eq!(normalize_exit(143), 143);
        assert_eq!(normalize_exit(129), 143);
        assert_eq!(normalize_exit(131), 143);
        assert_eq!(normalize_exit(0), 0);
    }

    #[test]
    fn a_missing_path_should_be_roots_unreadable_with_its_errno() {
        let missing = PathBuf::from("/definitely/not/here");
        let err = resolve_display_roots(std::slice::from_ref(&missing)).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.text().contains("/definitely/not/here"),
            "{}",
            err.text()
        );
    }

    #[test]
    fn a_file_should_not_be_a_display_root_and_a_duplicate_should_collapse() {
        let d = std::env::temp_dir();
        let got = resolve_display_roots(&[d.clone(), d.clone()]).unwrap();
        assert_eq!(got.len(), 1);
        let file = std::path::Path::new("/proc/self/mountinfo").to_path_buf();
        assert!(resolve_display_roots(&[file]).is_err());
    }
}

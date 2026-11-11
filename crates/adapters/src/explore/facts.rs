//! `ClassifyFactsLoader` (Task 6.3.1d): the two things the pure hint cannot read, loaded on a background
//! thread and sent as `ScanMsg::Facts` (N-17). The resolved cache paths come from `RootSet::allowed(Cache)`
//! after asking Go and pip for their own directories through the `CommandRunner`; the Steam facts come from
//! the same parser the cleaner's Steam kinds use (`SteamInstall::load`). Read-only, and every failure is
//! absence: no Steam facts leave Steam rows at `Shape`, a failed `go env` leaves the table paths.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;

use disk_cleanup_core::delete_types::AllowedTarget;
use disk_cleanup_core::explore::msg::{ClassifyFacts, ScanMsg, SteamLibraryFacts};
use disk_cleanup_core::ids::AppId;
use disk_cleanup_core::ports::CommandRunner;
use disk_cleanup_core::roots::{CacheTool, RootSet};
use disk_cleanup_core::Kind;

use crate::discover_cache::ask_tool;
use crate::steam::SteamInstall;

pub struct ClassifyFactsLoader {
    roots: RootSet,
    steam_roots: Vec<PathBuf>,
    runner: Arc<dyn CommandRunner>,
}

impl ClassifyFactsLoader {
    /// `roots` are the cleaner's own (never the explore PATH); `steam_roots` are the Steam installs to read
    /// (`steam::default_steam_roots`).
    pub fn new(
        roots: RootSet,
        steam_roots: Vec<PathBuf>,
        runner: Arc<dyn CommandRunner>,
    ) -> ClassifyFactsLoader {
        ClassifyFactsLoader {
            roots,
            steam_roots,
            runner,
        }
    }

    /// The facts, synchronously. A part that cannot be read is absent, never an error.
    pub fn load(&self) -> ClassifyFacts {
        ClassifyFacts {
            cache_paths: self.cache_paths(),
            steam: self.steam(),
        }
    }

    /// Loads on a thread of its own and sends the result; the caller never waits for it.
    pub fn spawn(self, tx: Sender<ScanMsg>) -> std::io::Result<JoinHandle<()>> {
        std::thread::Builder::new()
            .name("explore-facts".into())
            .spawn(move || {
                // A panic leaves the defaults: no Steam facts, so Steam rows stay `Shape`.
                let facts = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.load()))
                    .unwrap_or_default();
                let _ = tx.send(ScanMsg::Facts(facts));
            })
    }

    fn cache_paths(&self) -> Vec<PathBuf> {
        let mut roots = self.roots.clone();
        for tool in [CacheTool::Go, CacheTool::Pip] {
            if let Some(reported) = ask_tool(&*self.runner, tool) {
                // A path the roots refuse stays out of the facts.
                let _ = roots.add_command_cache(tool, &reported);
            }
        }
        roots
            .allowed(Kind::Cache)
            .into_iter()
            .filter_map(|t| match t {
                AllowedTarget::Exact(p) => Some(p),
                AllowedTarget::Within { .. } => None,
            })
            .collect()
    }

    /// `None` when any install failed to parse: a partial installed set would call an installed game's
    /// prefix an orphan. No Steam install at all is an empty, loaded set.
    fn steam(&self) -> Option<Vec<SteamLibraryFacts>> {
        let mut out: Vec<SteamLibraryFacts> = Vec::new();
        for root in &self.steam_roots {
            let install = SteamInstall::load(root).ok()?;
            for lib in &install.libraries {
                if out.iter().any(|l| &l.root == lib) {
                    continue;
                }
                let apps = install.installed.iter().filter(|a| &a.library == lib);
                out.push(SteamLibraryFacts {
                    root: lib.clone(),
                    mounted: true,
                    installed: apps.clone().map(|a| AppId(u64::from(a.appid))).collect(),
                    install_dirs: apps.map(|a| OsString::from(&a.installdir)).collect(),
                });
            }
        }
        Some(out)
    }
}

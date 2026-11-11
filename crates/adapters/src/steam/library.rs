//! Libraries and the installed set. Every parse problem fails closed: a partial installed set would turn an
//! installed game's prefix into an "orphan".

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use disk_cleanup_core::ports::SystemProbe;
use disk_cleanup_core::roots::SteamLibraryRoot;
use disk_cleanup_core::{Platform, SkipReason, Tri};
use steamlocate::{Error as SteamError, SteamDir};

/// Why the Steam kinds cannot be offered. `Skip` is a refusal of this run; `Unavailable` says the
/// discovery output cannot be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteamGate {
    Skip(SkipReason),
    Unavailable(SkipReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledApp {
    pub appid: u32,
    pub installdir: String,
    pub library: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamInstall {
    pub root: PathBuf,
    pub libraries: Vec<PathBuf>,
    pub installed: Vec<InstalledApp>,
}

impl SteamInstall {
    /// Reads `libraryfolders.vdf` and every library's `appmanifest_*.acf`.
    /// A listed library that is missing gives `Skip(LibraryUnmounted)` for the whole kind; any
    /// unparseable file gives `Unavailable(UnrecognisedOutput)`.
    pub fn load(root: &Path) -> Result<SteamInstall, SteamGate> {
        let unrecognised = SteamGate::Unavailable(SkipReason::UnrecognisedOutput);
        let dir = SteamDir::from_dir(root).map_err(|_| unrecognised.clone())?;
        let paths = dir.library_paths().map_err(|_| unrecognised.clone())?;
        if paths.is_empty() {
            return Err(unrecognised);
        }
        let mut libraries: Vec<PathBuf> = Vec::new();
        let mut installed = Vec::new();
        for lib in dir.libraries().map_err(|_| unrecognised.clone())? {
            let lib = match lib {
                Ok(l) => l,
                Err(SteamError::Io { ref inner, .. })
                    if inner.kind() == std::io::ErrorKind::NotFound =>
                {
                    return Err(SteamGate::Skip(SkipReason::LibraryUnmounted));
                }
                Err(_) => return Err(unrecognised),
            };
            // Canonical, so the delete engine's lexical rules see the path the kernel does; a library
            // listed twice (`~/.steam/steam` and the real directory) is read once.
            let path = fs::canonicalize(lib.path()).unwrap_or_else(|_| lib.path().to_path_buf());
            if libraries.contains(&path) {
                continue;
            }
            // A mount point left behind by an unmounted drive has no `steamapps` inside.
            if !path.join("steamapps").is_dir() {
                return Err(SteamGate::Skip(SkipReason::LibraryUnmounted));
            }
            for app in lib.apps() {
                let app = app.map_err(|_| unrecognised.clone())?;
                installed.push(InstalledApp {
                    appid: app.app_id,
                    installdir: app.install_dir,
                    library: path.clone(),
                });
            }
            libraries.push(path);
        }
        installed.sort_by(|a, b| (a.appid, &a.library).cmp(&(b.appid, &b.library)));
        Ok(SteamInstall {
            root: root.to_path_buf(),
            libraries,
            installed,
        })
    }

    pub fn installed_ids(&self) -> BTreeSet<u32> {
        self.installed.iter().map(|a| a.appid).collect()
    }

    /// The installed app whose `installdir` names `dir_name`, compared case-insensitively, in any library.
    pub fn app_for_dir(&self, dir_name: &OsStr) -> Option<&InstalledApp> {
        let name = dir_name.to_str()?.to_lowercase();
        self.installed
            .iter()
            .find(|a| a.installdir.to_lowercase() == name)
    }

    /// An empty installed set while `compatdata` has entries means the libraries were not read, not that
    /// every prefix is an orphan.
    pub fn check_against_compatdata(&self, compatdata_entries: usize) -> Result<(), SteamGate> {
        if self.installed.is_empty() && compatdata_entries > 0 {
            return Err(SteamGate::Unavailable(SkipReason::LibraryUnmounted));
        }
        Ok(())
    }

    /// The same rule over everything a Steam kind could offer: the entries of `compatdata`, `shadercache`
    /// and `common` of every library.
    pub fn check_installed_set(&self) -> Result<(), SteamGate> {
        let entries = self
            .libraries
            .iter()
            .flat_map(|l| {
                ["compatdata", "shadercache", "common"]
                    .map(|sub| count_entries(&l.join("steamapps").join(sub)))
            })
            .sum();
        self.check_against_compatdata(entries)
    }
}

fn count_entries(dir: &Path) -> usize {
    fs::read_dir(dir).map_or(0, Iterator::count)
}

/// The libraries `libraryfolders.vdf` lists that are missing or hold no `steamapps` (an unmounted drive).
pub fn unmounted_libraries(root: &Path) -> Vec<PathBuf> {
    let Ok(dir) = SteamDir::from_dir(root) else {
        return Vec::new();
    };
    dir.library_paths()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| !p.join("steamapps").is_dir())
        .collect()
}

/// Every library of the install at `root` as a delete bound: canonical, `mounted` when it holds `steamapps`.
/// Nothing is parsed beyond the library list, so this works when `SteamInstall::load` fails closed.
pub fn library_roots(root: &Path) -> Vec<SteamLibraryRoot> {
    let Ok(dir) = SteamDir::from_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<SteamLibraryRoot> = Vec::new();
    for p in dir.library_paths().unwrap_or_default() {
        let path = fs::canonicalize(&p).unwrap_or(p);
        if out.iter().any(|l| l.path == path) {
            continue;
        }
        let mounted = path.join("steamapps").is_dir();
        out.push(SteamLibraryRoot { path, mounted });
    }
    out
}

/// True when any library's `steamapps/downloading` holds an entry, or cannot be read.
pub fn downloading_nonempty(install: &SteamInstall) -> bool {
    install.libraries.iter().any(|l| {
        let d = l.join("steamapps/downloading");
        match fs::read_dir(&d) {
            Ok(mut it) => it.next().is_some(),
            Err(e) => e.kind() != std::io::ErrorKind::NotFound,
        }
    })
}

/// The whole gate, also used for the re-check at execute time: Steam running (or not known to be
/// stopped), then libraries and manifests, then downloads in progress.
pub fn plan_gate(probe: &dyn SystemProbe, root: &Path) -> Result<SteamInstall, SteamGate> {
    if probe.process_named("steam") != Tri::NotRunning {
        return Err(SteamGate::Skip(SkipReason::SteamRunning));
    }
    let install = SteamInstall::load(root)?;
    if downloading_nonempty(&install) {
        return Err(SteamGate::Skip(SkipReason::SteamRunning));
    }
    Ok(install)
}

/// Existing Steam roots for `platform`, in preference order (Linux native, Flatpak; macOS), without
/// duplicates that resolve to the same directory.
pub fn default_steam_roots(home: &Path, platform: Platform) -> Vec<PathBuf> {
    let candidates: Vec<PathBuf> = match platform {
        Platform::Macos => vec![home.join("Library/Application Support/Steam")],
        Platform::Linux => vec![
            home.join(".local/share/Steam"),
            home.join(".steam/steam"),
            home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
        ],
    };
    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .filter(|p| p.is_dir())
        .filter(|p| seen.insert(fs::canonicalize(p).unwrap_or_else(|_| p.clone())))
        .collect()
}

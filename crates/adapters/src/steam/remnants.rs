//! Uninstalled-game remnants (Story 4.2.2): the directories of `steamapps/common` that no `installdir`, in
//! any library, names. Listing only.

use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;

use super::SteamInstall;

/// One directory of `<library>/steamapps/common` that no installed app names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemnantEntry {
    pub name: String,
    pub path: PathBuf,
    pub library: PathBuf,
}

/// Remnants of every library, sorted by path. The caller has already established that every library in
/// `libraryfolders.vdf` is mounted (`SteamInstall::load` fails closed otherwise), because the manifest of a
/// game in `common` may live in any of them.
///
/// Files, and symlinks that do not lead to a directory, are never candidates. A symlink to a directory is
/// listed so that the guard can refuse it by name. A name that is not UTF-8 cannot be compared with an
/// `installdir`, so it is not offered.
pub fn list_remnants(install: &SteamInstall) -> Vec<RemnantEntry> {
    let mut out = Vec::new();
    for lib in &install.libraries {
        let dir = lib.join("steamapps/common");
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let file_name = e.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let path = e.path();
            let is_dir = match e.file_type() {
                Ok(t) if t.is_dir() => true,
                Ok(t) if t.is_symlink() => fs::metadata(&path).is_ok_and(|m| m.is_dir()),
                _ => false,
            };
            if is_dir && is_remnant(install, OsStr::new(name)) {
                out.push(RemnantEntry {
                    name: name.to_string(),
                    path,
                    library: lib.clone(),
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// No `installdir` of any library names `dir_name`, compared case-insensitively.
pub fn is_remnant(install: &SteamInstall, dir_name: &OsStr) -> bool {
    dir_name.to_str().is_some() && install.app_for_dir(dir_name).is_none()
}

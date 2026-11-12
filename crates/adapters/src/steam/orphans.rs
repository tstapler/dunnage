//! Orphan prefixes and shader caches (Story 4.2.2): the entries of `compatdata` and `shadercache` whose app
//! id no `appmanifest_*.acf` of any library names. Listing only; sizes, ages and the guard are the target's.

use std::fs;
use std::path::PathBuf;

use dunnage_core::{AppId, Kind};

use super::SteamInstall;

/// One directory of `compatdata` or `shadercache` that no installed app owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanEntry {
    pub kind: Kind,
    pub app_id: AppId,
    pub path: PathBuf,
    pub library: PathBuf,
}

/// `compatdata/<appid>` and `shadercache/<appid>` of every library, orphans only, sorted by path.
///
/// An app id of 2^31 or more is a non-Steam shortcut and never an orphan. A name that is not all digits, a
/// file and anything that cannot be listed is not a candidate. A symlink is listed so that the guard can
/// refuse it by name.
pub fn list_orphans(install: &SteamInstall) -> Vec<OrphanEntry> {
    let installed = install.installed_ids();
    let mut out = Vec::new();
    for lib in &install.libraries {
        for (sub, kind) in [
            ("compatdata", Kind::SteamPrefix),
            ("shadercache", Kind::SteamShaderCache),
        ] {
            let dir = lib.join("steamapps").join(sub);
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let Some(app_id) = e.file_name().to_str().and_then(parse_app_id) else {
                    continue;
                };
                if app_id.is_non_steam_shortcut() || installed.contains(&u32_of(app_id)) {
                    continue;
                }
                if !e.file_type().is_ok_and(|t| t.is_dir() || t.is_symlink()) {
                    continue;
                }
                out.push(OrphanEntry {
                    kind,
                    app_id,
                    path: e.path(),
                    library: lib.clone(),
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Whether `app_id` is still unowned in `install`; the execute-time form of the orphan rule.
pub fn is_orphan(install: &SteamInstall, app_id: AppId) -> bool {
    !app_id.is_non_steam_shortcut() && !install.installed_ids().contains(&u32_of(app_id))
}

/// All-digit names only (no sign, no space), which is also what rule F's `SteamAppId` pattern admits.
fn parse_app_id(name: &str) -> Option<AppId> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse::<u64>().ok().map(AppId)
}

/// App ids below 2^31 fit; larger ones were already ruled out as shortcuts.
fn u32_of(a: AppId) -> u32 {
    u32::try_from(a.0).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_app_id_should_accept_only_digits() {
        assert_eq!(parse_app_id("99999"), Some(AppId(99_999)));
        assert_eq!(parse_app_id("3000000000"), Some(AppId(3_000_000_000)));
        assert_eq!(parse_app_id(""), None);
        assert_eq!(parse_app_id("-1"), None);
        assert_eq!(parse_app_id("12a"), None);
        assert_eq!(parse_app_id("99999999999999999999999"), None);
    }
}

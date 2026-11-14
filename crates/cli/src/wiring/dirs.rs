//! The one place that reads `$HOME` and the XDG directories, from environment variables only, never from
//! the passwd entry (`getpwuid`). `scripts/check-home-sources.sh` fails if any other file reads a home.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use dunnage_core::{Dirs, Identity};

/// Usage error (exit 64): the home directory cannot be established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirsError {
    HomeNotSet,
    HomeInvalid { path: PathBuf },
}

impl DirsError {
    pub fn exit_code(&self) -> i32 {
        match self {
            DirsError::HomeNotSet | DirsError::HomeInvalid { .. } => 64,
        }
    }

    pub fn text(&self) -> String {
        match self {
            DirsError::HomeNotSet => "error: HOME is not set. Nothing was scanned. Set HOME to your home directory (dunnage never reads the passwd entry), then run again.".into(),
            DirsError::HomeInvalid { path } => format!(
                "error: HOME ({}) is not an absolute path to an existing directory. Nothing was scanned. Fix HOME, then run again.",
                path.display()
            ),
        }
    }
}

impl std::fmt::Display for DirsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text())
    }
}

impl std::error::Error for DirsError {}

/// The state directory name before the rename (Story 5.3.0). Audit logs in it are append-only evidence:
/// the tool never moves them, it only tells the owner which `mv` to run.
pub const LEGACY_STATE_DIR: &str = "disk-cleanup";

/// Which of the old and new state directories exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyState {
    /// No old-name directory, or the new one is present alone.
    Clear,
    OldOnly {
        old: PathBuf,
        new: PathBuf,
    },
    Both {
        old: PathBuf,
        new: PathBuf,
    },
}

impl LegacyState {
    pub fn note(&self) -> Option<String> {
        match self {
            LegacyState::OldOnly { old, new } => Some(format!(
                "NOTE: the state directory was renamed. Your audit log is in {old}. Run 'mv {old} {new}' to keep it (dunnage never moves audit logs itself).",
                old = old.display(),
                new = new.display()
            )),
            _ => None,
        }
    }

    /// Exit 64 text for an apply when both directories exist.
    pub fn apply_refusal(&self) -> Option<String> {
        match self {
            LegacyState::Both { old, new } => Some(format!(
                "error: both {} and {} exist, so dunnage cannot tell which audit log is current. Nothing was deleted. Merge them or remove the one you do not need, then run again.",
                old.display(),
                new.display()
            )),
            _ => None,
        }
    }
}

/// `exists` must not follow symlinks.
pub fn legacy_state(base: &Dirs, exists: &dyn Fn(&Path) -> bool) -> LegacyState {
    let (old, new) = (
        base.state.join(LEGACY_STATE_DIR),
        base.state.join("dunnage"),
    );
    match (exists(&old), exists(&new)) {
        (true, true) => LegacyState::Both { old, new },
        (true, false) => LegacyState::OldOnly { old, new },
        _ => LegacyState::Clear,
    }
}

pub fn real_legacy_state(base: &Dirs) -> LegacyState {
    legacy_state(base, &|p| std::fs::symlink_metadata(p).is_ok())
}

/// A set, non-empty, absolute value; the XDG spec says to ignore anything else.
fn xdg(
    env: &dyn Fn(&str) -> Option<OsString>,
    name: &str,
    fallback: impl FnOnce() -> PathBuf,
) -> PathBuf {
    match env(name) {
        Some(v) if Path::new(&v).is_absolute() => PathBuf::from(v),
        _ => fallback(),
    }
}

/// `Dirs.home` is the canonical form of `$HOME`; the XDG directories default to `<home>/.local/state`,
/// `<home>/.cache` and `<home>/.config`.
pub fn resolve_dirs(
    env: &dyn Fn(&str) -> Option<OsString>,
    canon: &dyn Fn(&Path) -> io::Result<(PathBuf, Identity)>,
) -> Result<Dirs, DirsError> {
    let raw = env("HOME")
        .filter(|v| !v.is_empty())
        .ok_or(DirsError::HomeNotSet)?;
    let raw = PathBuf::from(raw);
    if !raw.is_absolute() {
        return Err(DirsError::HomeInvalid { path: raw });
    }
    let (home, _) = canon(&raw).map_err(|_| DirsError::HomeInvalid { path: raw })?;
    Ok(Dirs {
        state: xdg(env, "XDG_STATE_HOME", || home.join(".local/state")),
        cache: xdg(env, "XDG_CACHE_HOME", || home.join(".cache")),
        config: xdg(env, "XDG_CONFIG_HOME", || home.join(".config")),
        home,
    })
}

/// `realpath` plus `stat`, and only for a directory: a file or a dangling path is an error.
pub fn real_canon(path: &Path) -> io::Result<(PathBuf, Identity)> {
    let canonical = std::fs::canonicalize(path)?;
    let meta = std::fs::metadata(&canonical)?;
    if !meta.is_dir() {
        return Err(io::Error::from(io::ErrorKind::NotADirectory));
    }
    Ok((canonical, Identity::of(&meta)))
}

pub fn process_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// The production resolution: the process environment and the real filesystem.
pub fn resolve_real_dirs() -> Result<Dirs, DirsError> {
    resolve_dirs(&process_env, &real_canon)
}

/// `CARGO_HOME` as the environment gives it, for `RootSet::with_cargo_home`: unset or empty is `None` (the
/// default `~/.cargo`). It is not canonicalised: a link anywhere on the way is refused by the `cargo-home`
/// target, not followed.
pub fn cargo_home_env() -> Option<std::path::PathBuf> {
    std::env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unnecessary_wraps,
    non_snake_case
)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let m: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), OsString::from(v)))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn canon_ok(p: &Path) -> io::Result<(PathBuf, Identity)> {
        Ok((p.to_path_buf(), Identity { dev: 1, ino: 2 }))
    }

    #[test]
    fn resolve_dirs_should_use_canonical_home_and_xdg_defaults_When_xdg_unset() {
        let env = env_of(&[("HOME", "/fx")]);
        let canon = |p: &Path| -> io::Result<(PathBuf, Identity)> {
            assert_eq!(p, Path::new("/fx"));
            Ok(("/real/fx".into(), Identity { dev: 1, ino: 2 }))
        };
        let d = resolve_dirs(&env, &canon).unwrap();
        assert_eq!(d.home, Path::new("/real/fx"));
        assert_eq!(d.state, Path::new("/real/fx/.local/state"));
        assert_eq!(d.cache, Path::new("/real/fx/.cache"));
        assert_eq!(d.config, Path::new("/real/fx/.config"));
    }

    #[test]
    fn resolve_dirs_should_honour_absolute_xdg_values_and_ignore_relative_or_empty_ones() {
        let env = env_of(&[
            ("HOME", "/fx"),
            ("XDG_STATE_HOME", "/xdg/state"),
            ("XDG_CACHE_HOME", "relative/cache"),
            ("XDG_CONFIG_HOME", ""),
        ]);
        let d = resolve_dirs(&env, &canon_ok).unwrap();
        assert_eq!(d.state, Path::new("/xdg/state"));
        assert_eq!(d.cache, Path::new("/fx/.cache"));
        assert_eq!(d.config, Path::new("/fx/.config"));
    }

    #[test]
    fn resolve_dirs_should_exit_64_When_home_unset_missing_or_not_a_directory_and_never_use_getpwuid(
    ) {
        let unset = resolve_dirs(&env_of(&[]), &canon_ok).unwrap_err();
        assert_eq!(unset, DirsError::HomeNotSet);
        assert_eq!(unset.exit_code(), 64);
        assert!(unset.text().contains("HOME is not set"));
        assert_eq!(
            resolve_dirs(&env_of(&[("HOME", "")]), &canon_ok).unwrap_err(),
            DirsError::HomeNotSet
        );
        let missing = |_: &Path| -> io::Result<(PathBuf, Identity)> {
            Err(io::Error::from(io::ErrorKind::NotFound))
        };
        let e = resolve_dirs(&env_of(&[("HOME", "/nope")]), &missing).unwrap_err();
        assert_eq!(
            e,
            DirsError::HomeInvalid {
                path: "/nope".into()
            }
        );
        assert_eq!(e.exit_code(), 64);
        let e = resolve_dirs(&env_of(&[("HOME", "relative")]), &canon_ok).unwrap_err();
        assert!(matches!(e, DirsError::HomeInvalid { .. }));
    }

    #[test]
    fn real_canon_should_resolve_a_directory_and_refuse_a_file_or_a_missing_path() {
        let d = tempfile::tempdir().unwrap();
        let (p, id) = real_canon(d.path()).unwrap();
        assert!(p.is_absolute());
        assert_ne!(id.ino, 0);
        let f = d.path().join("f");
        std::fs::write(&f, "x").unwrap();
        assert!(real_canon(&f).is_err());
        assert!(real_canon(&d.path().join("missing")).is_err());
    }

    #[test]
    fn legacy_state_should_note_old_only_and_refuse_both() {
        let base = Dirs {
            home: "/h".into(),
            state: "/s".into(),
            cache: "/c".into(),
            config: "/f".into(),
        };
        let only = |old: bool, new: bool| {
            legacy_state(&base, &move |p| {
                (old && p.ends_with("disk-cleanup")) || (new && p.ends_with("dunnage"))
            })
        };
        assert_eq!(only(false, false), LegacyState::Clear);
        assert_eq!(only(false, true), LegacyState::Clear);
        let o = only(true, false);
        assert!(o.note().unwrap().contains("mv /s/disk-cleanup /s/dunnage"));
        assert!(o.apply_refusal().is_none());
        let b = only(true, true);
        assert!(b.note().is_none());
        assert!(b
            .apply_refusal()
            .unwrap()
            .contains("both /s/disk-cleanup and /s/dunnage exist"));
    }
}

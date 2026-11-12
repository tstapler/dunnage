//! `$XDG_CONFIG_HOME/dunnage/config.toml` reader (Task 1.1.5b). Only `roots` is read; other keys are
//! ignored so a newer file does not break an older binary. A missing file is the empty config.

use std::path::{Path, PathBuf};

use dunnage_core::Dirs;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// `None` when the key is absent: the defaults apply. `Some(vec![])` scans nothing.
    pub roots: Option<Vec<PathBuf>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("config {} is not valid TOML: {message}", path.display())]
    Parse { path: PathBuf, message: String },
    #[error("config {}: `roots` must be an array of strings", path.display())]
    RootsShape { path: PathBuf },
}

/// `<config>/dunnage/config.toml`; a config that still sits under the pre-rename `disk-cleanup` directory is
/// read from there until the owner moves it.
pub fn config_path(dirs: &Dirs) -> PathBuf {
    let new = dirs.config.join("dunnage").join("config.toml");
    let old = dirs.config.join("disk-cleanup").join("config.toml");
    if !new.exists() && old.exists() {
        return old;
    }
    new
}

pub fn read_config(path: &Path) -> Result<Config, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    parse_config(path, &text)
}

pub fn parse_config(path: &Path, text: &str) -> Result<Config, ConfigError> {
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.message().to_string(),
        })?;
    let Some(roots) = table.get("roots") else {
        return Ok(Config::default());
    };
    let shape = || ConfigError::RootsShape {
        path: path.to_path_buf(),
    };
    let items = roots.as_array().ok_or_else(shape)?;
    let roots = items
        .iter()
        .map(|v| v.as_str().map(PathBuf::from).ok_or_else(shape))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Config { roots: Some(roots) })
}

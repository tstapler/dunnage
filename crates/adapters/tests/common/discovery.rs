//! Fixture helpers shared by the discovery tests. Everything lives in a tempdir under a fake `$HOME`.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use dunnage_core::roots::RootSet;
use dunnage_core::{Dirs, Identity};

pub struct Fixture {
    pub _tmp: tempfile::TempDir,
    pub home: PathBuf,
    pub code: PathBuf,
}

pub fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let code = home.join("code");
    fs::create_dir_all(&code).unwrap();
    Fixture {
        _tmp: tmp,
        home,
        code,
    }
}

pub fn dirs_of(home: &Path) -> Dirs {
    Dirs {
        home: home.to_path_buf(),
        state: home.join(".local/state"),
        cache: home.join(".cache"),
        config: home.join(".config"),
    }
}

pub fn canon(p: &Path) -> std::io::Result<(PathBuf, Identity)> {
    let c = p.canonicalize()?;
    let m = fs::metadata(&c)?;
    Ok((c, Identity::of(&m)))
}

pub fn roots_for(f: &Fixture) -> RootSet {
    RootSet::resolve(
        &dirs_of(&f.home),
        std::slice::from_ref(&f.code),
        None,
        &f.home,
        &canon,
    )
    .unwrap()
}

pub fn now_epoch() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

/// Sets the mtime of a file or directory to `age` ago.
pub fn set_age(p: &Path, age: Duration) {
    let t = SystemTime::now() - age;
    File::open(p).unwrap().set_modified(t).unwrap();
}

pub const HOUR: Duration = Duration::from_hours(1);
pub const DAY: Duration = Duration::from_hours(24);

/// `<checkout>/package.json`, an optional lockfile and `<checkout>/node_modules/x`.
pub fn node_checkout(dir: &Path, lock: Option<&str>) -> PathBuf {
    fs::create_dir_all(dir.join("node_modules")).unwrap();
    fs::write(dir.join("package.json"), "{}").unwrap();
    if let Some(l) = lock {
        fs::write(dir.join("package-lock.json"), l).unwrap();
    }
    fs::write(dir.join("node_modules/x"), "x").unwrap();
    dir.join("node_modules")
}

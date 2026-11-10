//! The one rule that decides which directory names are rebuildable build dirs.
//! `scan.rs` and `classify_path` both call [`classify_dir`]; nothing else decides these names.

use crate::scan::BuildKind;
use std::ffi::OsStr;
use std::path::Path;

/// Marker files found next to a directory, regular files only (symlinks do not count).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParentFacts {
    pub has_package_json: bool,
    pub has_cargo_toml: bool,
}

/// Pure: no filesystem access.
pub fn classify_dir(name: &OsStr, parent: ParentFacts) -> Option<BuildKind> {
    match name.to_str()? {
        "node_modules" if parent.has_package_json => Some(BuildKind::NodeModules),
        "target" if parent.has_cargo_toml => Some(BuildKind::CargoTarget),
        _ => None,
    }
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// Classifies `path` by looking at its parent directory's marker files.
pub fn classify_path(path: &Path) -> Option<BuildKind> {
    let name = path.file_name()?;
    let parent = path.parent()?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    classify_dir(
        name,
        ParentFacts {
            has_package_json: is_regular_file(&parent.join("package.json")),
            has_cargo_toml: is_regular_file(&parent.join("Cargo.toml")),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dc-classify-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn classify_path_node_modules_and_other() {
        let d = tmp("nm");
        let app = d.join("app");
        fs::create_dir_all(app.join("node_modules")).unwrap();
        fs::create_dir_all(app.join("other")).unwrap();
        fs::write(app.join("package.json"), "{}").unwrap();
        assert_eq!(
            classify_path(&app.join("node_modules")),
            Some(BuildKind::NodeModules)
        );
        assert_eq!(classify_path(&app.join("other")), None);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn classify_dir_is_pure_and_uses_both_flags() {
        let none = ParentFacts::default();
        let pj = ParentFacts {
            has_package_json: true,
            has_cargo_toml: false,
        };
        let ct = ParentFacts {
            has_package_json: false,
            has_cargo_toml: true,
        };
        assert_eq!(
            classify_dir(OsStr::new("node_modules"), pj),
            Some(BuildKind::NodeModules)
        );
        assert_eq!(classify_dir(OsStr::new("node_modules"), ct), None);
        assert_eq!(classify_dir(OsStr::new("node_modules"), none), None);
        assert_eq!(
            classify_dir(OsStr::new("target"), ct),
            Some(BuildKind::CargoTarget)
        );
        assert_eq!(classify_dir(OsStr::new("target"), pj), None);
        assert_eq!(classify_dir(OsStr::new("src"), pj), None);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_marker_gives_none_in_classify_path() {
        let d = tmp("sym");
        let app = d.join("app");
        fs::create_dir_all(app.join("target")).unwrap();
        fs::write(d.join("real.toml"), "").unwrap();
        std::os::unix::fs::symlink(d.join("real.toml"), app.join("Cargo.toml")).unwrap();
        assert_eq!(classify_path(&app.join("target")), None);
        let _ = fs::remove_dir_all(&d);
    }
}

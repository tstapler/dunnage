//! Delete-time re-check of one build dir (Story 3.2.1): gathers fresh facts through `DirOps` (the entry's
//! own `fstatat`, so a nested subvolume shows its own `st_dev`), then asks the pure `decide_build_dir`.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use disk_cleanup_core::guard::Facts;
use disk_cleanup_core::ports::SystemProbe;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::targets::build_dir::{
    decide_build_dir, effective_min_age, BuildFacts, BuildGuard, Reproducibility,
};
use disk_cleanup_core::{
    BuildDirFlavor, DeleteError, FileKind, Identity, Kind, RootDev, SkipReason,
};
use disk_cleanup_scan::classify::classify_path;
use disk_cleanup_scan::scan::BuildKind;

use crate::dirops::{open_parent, open_verified_root, DirOps};
use crate::discover_build::symlink_between;
use crate::mtime_probe::{
    probe_newest_mtime, to_system_time, unix_secs, NewestStore, ProbeTarget, Recency,
};

const LOCKFILES: [&str; 4] = [
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
];

/// The decision plus the newest mtime the probe found (for the plan row), when it looked.
#[derive(Debug)]
pub struct BuildRecheck {
    pub guard: BuildGuard,
    pub newest_mtime: Option<i64>,
}

fn skip(r: SkipReason) -> BuildRecheck {
    BuildRecheck {
        guard: BuildGuard::Skip {
            reason: r,
            detail: None,
        },
        newest_mtime: None,
    }
}

fn kind_of(f: BuildDirFlavor) -> Kind {
    match f {
        BuildDirFlavor::NodeModules => Kind::NodeModules,
        BuildDirFlavor::CargoTarget => Kind::CargoTarget,
    }
}

fn marker_present(path: &Path, flavor: BuildDirFlavor) -> bool {
    matches!(
        (classify_path(path), flavor),
        (Some(BuildKind::NodeModules), BuildDirFlavor::NodeModules)
            | (Some(BuildKind::CargoTarget), BuildDirFlavor::CargoTarget)
    )
}

/// Lexical `..` and `.` collapse; never touches the filesystem.
fn collapse(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// True when the symlink `link` points outside `project`. `lstat` and `readlink` only: the target is never
/// followed, so the verdict does not depend on what the target is.
fn link_escapes(link: &Path, project: &Path) -> bool {
    let Ok(target) = std::fs::read_link(link) else {
        return true;
    };
    let abs = if target.is_absolute() {
        target
    } else {
        link.parent().unwrap_or(project).join(target)
    };
    !collapse(&abs).starts_with(project)
}

fn dir_entries(dir: &Path) -> Vec<(std::ffi::OsString, bool)> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| {
                    let is_link = e.file_type().is_ok_and(|t| t.is_symlink());
                    (e.file_name(), is_link)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `npm install` could recreate `node_modules`: no top-level (or `@scope/` or `.bin`) symlink
/// leaves the project, and a lockfile sits next to it.
fn node_modules_reproducible(nm: &Path) -> Reproducibility {
    let Some(project) = nm.parent() else {
        return Reproducibility::NotChecked;
    };
    let mut to_check: Vec<PathBuf> = vec![nm.to_path_buf(), nm.join(".bin")];
    for (name, is_link) in dir_entries(nm) {
        let p = nm.join(&name);
        if is_link {
            if link_escapes(&p, project) {
                return Reproducibility::LinkedOutside;
            }
        } else if name.to_str().is_some_and(|n| n.starts_with('@')) {
            to_check.push(p);
        }
    }
    for dir in to_check {
        for (name, is_link) in dir_entries(&dir) {
            if is_link && link_escapes(&dir.join(name), project) {
                return Reproducibility::LinkedOutside;
            }
        }
    }
    let has_lock = LOCKFILES
        .iter()
        .any(|l| std::fs::symlink_metadata(project.join(l)).is_ok_and(|m| m.file_type().is_file()));
    if has_lock {
        Reproducibility::Reproducible
    } else {
        Reproducibility::NoLockfile
    }
}

/// What the re-check needs from outside.
pub struct RecheckEnv<'a, D> {
    pub ops: &'a D,
    pub roots: &'a RootSet,
    pub probe: &'a dyn SystemProbe,
}

/// Re-validates `path` (a build dir planned with identity `planned`) against fresh state. `now` is Unix
/// seconds and `min_age` the `--min-age` flag; the 60-minute floor is applied by the pure decision.
pub fn recheck_build_dir<D: DirOps>(
    env: &RecheckEnv<'_, D>,
    flavor: BuildDirFlavor,
    path: &Path,
    planned: Identity,
    now: i64,
    min_age: Duration,
    store: &mut NewestStore,
) -> BuildRecheck {
    let Some(root) = env
        .roots
        .scan_roots()
        .iter()
        .find(|r| path.starts_with(r) && path.as_os_str() != r.as_os_str())
    else {
        return skip(SkipReason::OutsideRoot);
    };
    let (Some(name), Some(parent_abs)) = (path.file_name(), path.parent()) else {
        return skip(SkipReason::OutsideRoot);
    };
    let Ok(rel_parent) = parent_abs.strip_prefix(root) else {
        return skip(SkipReason::OutsideRoot);
    };
    let Ok(root_meta) = std::fs::metadata(root) else {
        return skip(SkipReason::OutsideRoot);
    };
    let opened = open_verified_root(env.ops, root, Identity::of(&root_meta))
        .and_then(|vr| open_parent(env.ops, &vr, rel_parent).map(|p| (vr, p)));
    let (vroot, parent) = match opened {
        Ok(x) => x,
        Err(DeleteError::Symlink) => return skip(SkipReason::Symlink),
        Err(DeleteError::Io(libc::ENOENT)) => return skip(SkipReason::Gone),
        Err(DeleteError::IdentityMismatch) => return skip(SkipReason::IdentityChanged),
        Err(_) => return skip(SkipReason::OutsideRoot),
    };
    let st = match env.ops.stat_at(parent.dir(), name) {
        Ok(st) => st,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return skip(SkipReason::Gone),
        Err(_) => return skip(SkipReason::IdentityChanged),
    };
    let root_dev = env
        .ops
        .fstat(vroot.dir())
        .map_or(RootDev(root_meta_dev(&root_meta)), |s| RootDev(s.ident.dev));
    let mounts = env.probe.mounts().unwrap_or_default();
    let path_facts = Facts {
        kind: kind_of(flavor),
        path: path.to_path_buf(),
        ident: st.ident,
        is_symlink: st.kind == FileKind::Symlink || symlink_between(root, path),
        root_dev,
        is_mount_point: mounts.iter().any(|m| m.mount_point == path),
    };
    let marker = marker_present(path, flavor);
    let own = (st.mtime_secs, st.mtime_nsec);
    let (eff, _) = effective_min_age(min_age);
    let floor_secs = i64::try_from(eff.as_secs()).unwrap_or(i64::MAX);

    // The expensive probes run only when every cheaper guard already passed.
    let cheap_ok = st.kind == FileKind::Dir
        && !path_facts.is_symlink
        && st.ident == planned
        && marker
        && st.ident.on_dev(root_dev)
        && !path_facts.is_mount_point;
    let newest = if !cheap_ok {
        None
    } else if now - own.0 < floor_secs {
        Some(own.0)
    } else {
        let target = ProbeTarget {
            path,
            ident: st.ident,
            own,
            root_dev,
        };
        let floor = to_system_time(now - floor_secs, 0);
        match probe_newest_mtime(env.ops, target, floor, store) {
            Ok(p) => match p.recency {
                Recency::Young { newest, .. } => Some(unix_secs(newest)),
                Recency::Old { newest } => Some(newest.map_or(own.0, unix_secs)),
                Recency::Unknown { .. } => None,
            },
            Err(DeleteError::IdentityMismatch) => return skip(SkipReason::IdentityChanged),
            Err(DeleteError::Io(libc::ENOENT)) => return skip(SkipReason::Gone),
            Err(_) => None,
        }
    };
    let reproducibility = if flavor == BuildDirFlavor::NodeModules && cheap_ok && newest.is_some() {
        node_modules_reproducible(path)
    } else {
        Reproducibility::NotChecked
    };
    let facts = BuildFacts {
        path: path_facts,
        planned,
        marker_present: marker,
        newest_mtime: newest,
        reproducibility,
    };
    BuildRecheck {
        guard: decide_build_dir(&facts, env.roots, now, min_age),
        newest_mtime: newest,
    }
}

fn root_meta_dev(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.dev()
}

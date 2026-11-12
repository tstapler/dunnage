#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 5.1.3: `safety_` guarantees of the filesystem kinds against the scan-root set. The caches (Exact table
//! paths) and Steam (the library) are covered where their targets are tested; here build dirs and worktrees.

use std::io;
use std::path::{Path, PathBuf};

use dunnage_core::guard::Facts;
use dunnage_core::roots::RootSet;
use dunnage_core::targets::build_dir::{decide_build_dir, BuildFacts, BuildGuard, Reproducibility};
use dunnage_core::targets::worktree_guard::{
    decide_worktree, IgnoredFiles, PushState, WorktreeCandidate, WorktreeOpts,
};
use dunnage_core::{Dirs, Identity, Kind, RootDev, SkipReason, Tri};
use std::time::Duration;

const NOW: i64 = 1_000_000_000;
const DAY: i64 = 86_400;

fn roots() -> RootSet {
    let dirs = Dirs {
        home: "/h".into(),
        state: "/h/.local/state".into(),
        cache: "/h/.cache".into(),
        config: "/h/.config".into(),
    };
    let canon = |p: &Path| -> io::Result<(PathBuf, Identity)> {
        Ok((
            p.to_path_buf(),
            Identity {
                dev: 28,
                ino: p.as_os_str().len() as u64,
            },
        ))
    };
    RootSet::resolve(&dirs, &["/h/code".into()], None, Path::new("/h"), &canon).unwrap()
}

fn facts(kind: Kind, path: &str) -> Facts {
    Facts {
        kind,
        path: PathBuf::from(path),
        ident: Identity { dev: 28, ino: 7 },
        is_symlink: false,
        root_dev: RootDev(28),
        is_mount_point: false,
    }
}

#[test]
fn safety_out_of_root_build_dir_not_deleted() {
    // positive control: the same facts inside the scan root are allowed, so the skip below is the root rule's doing
    for (kind, path, inside) in [
        (
            Kind::NodeModules,
            "/elsewhere/app/node_modules",
            "/h/code/app/node_modules",
        ),
        (
            Kind::CargoTarget,
            "/h/other/app/target",
            "/h/code/app/target",
        ),
    ] {
        let probe = |path: &str| BuildFacts {
            path: facts(kind, path),
            planned: Identity { dev: 28, ino: 7 },
            marker_present: true,
            newest_mtime: Some(NOW - 90 * DAY),
            reproducibility: Reproducibility::Reproducible,
        };
        let g = decide_build_dir(&probe(inside), &roots(), NOW, Duration::from_secs(300));
        assert!(matches!(g, BuildGuard::Allow(_)), "control {inside}");
        let f = probe(path);
        let g = decide_build_dir(&f, &roots(), NOW, Duration::from_secs(300));
        match g {
            BuildGuard::Skip { reason, .. } => {
                assert_eq!(reason, SkipReason::OutsideRoot, "{path}");
            }
            BuildGuard::Allow(_) => {
                panic!("{path} is outside every scan root and must not be allowed")
            }
        }
    }
}

#[test]
fn safety_out_of_root_worktree_not_removed() {
    for path in [
        "/h/code/r/.claude/worktrees/agent-a41", // positive control, inside the root
        "/elsewhere/r/.claude/worktrees/agent-a41",
        "/h/other/r/.claude/worktrees/agent-a41",
    ] {
        let mut c = WorktreeCandidate::new(path, Some(NOW - 90 * DAY), false, false);
        c.push = PushState::Pushed;
        c.newest_mtime = Some(NOW - 90 * DAY);
        c.admin_mtime = Some(NOW - 90 * DAY);
        c.in_use = Tri::NotRunning;
        c.ignored = Some(IgnoredFiles::default());
        let o = WorktreeOpts {
            min_age: Duration::from_hours(336),
            discard_ignored: false,
        };
        let ident = Identity { dev: 28, ino: 7 };
        let g = decide_worktree(
            &c,
            Some(&facts(Kind::Worktree, path)),
            ident,
            &roots(),
            NOW,
            o,
        );
        let want = (!path.starts_with("/h/code")).then_some(SkipReason::OutsideRoot);
        assert_eq!(g.skip_reason(), want, "{path}");
    }
}

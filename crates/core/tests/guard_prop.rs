#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::io;
use std::path::{Path, PathBuf};

use disk_cleanup_core::guard::{decide_path_common, Facts, GuardDecision};
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::{Dirs, Identity, Kind, RootDev};
use proptest::prelude::*;

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

fn lexically_clean(s: &str) -> bool {
    s.strip_prefix('/').is_some_and(|r| {
        r.is_empty() || r.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    #[test]
    fn decide_path_common_should_never_Allow_When_10000_random_facts_are_unsafe(
        parts in proptest::collection::vec(
            prop_oneof!["h", "code", "app", "..", ".", "", ".git", "node_modules", "agent-1", "tmp"], 0..7),
        lead in any::<bool>(),
        trail in any::<bool>(),
        kind in prop_oneof![
            Just(Kind::NodeModules), Just(Kind::CargoTarget), Just(Kind::Worktree), Just(Kind::Cache)],
        is_symlink in any::<bool>(),
        dev in prop_oneof![Just(28u64), Just(34u64)],
        is_mount_point in any::<bool>(),
    ) {
        let mut s = if lead { "/".to_string() } else { String::new() };
        s.push_str(&parts.join("/"));
        if trail { s.push('/'); }
        let roots = roots();
        let outside_root = !roots.admits(kind, Path::new(&s));
        let facts = Facts {
            kind,
            path: PathBuf::from(&s),
            ident: Identity { dev, ino: 5 },
            is_symlink,
            root_dev: RootDev(28),
            is_mount_point,
        };
        let unsafe_facts = outside_root || is_symlink || dev != 28 || is_mount_point || !lexically_clean(&s);
        if unsafe_facts {
            prop_assert!(!matches!(decide_path_common(&facts, &roots), GuardDecision::Allow(_)), "{s}");
        }
    }
}

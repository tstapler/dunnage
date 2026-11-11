#![allow(clippy::unwrap_used, non_snake_case)]
//! Task 6.6.5h: a symlinked XDG state directory leaves the audit log unmarkable (review C7), on a real tree.

use disk_cleanup_cli::wiring::explore::canonical_lenient;
use disk_cleanup_core::explore::mark_check::OwnState;
use disk_cleanup_core::Dirs;

#[test]
fn safety_explorer_own_state_covers_the_real_directory_behind_a_symlinked_xdg_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(base.join("data/st/disk-cleanup")).unwrap();
    std::fs::create_dir_all(base.join("home/.local")).unwrap();
    std::os::unix::fs::symlink(base.join("data/st"), base.join("home/.local/state")).unwrap();
    let dirs = Dirs {
        home: base.join("home"),
        state: base.join("home/.local/state"),
        cache: base.join("home/.cache"),
        config: base.join("home/.config"),
    };
    let lexical = OwnState::new(&dirs, base.join("bin/dc"));
    let real_audit = base.join("data/st/disk-cleanup/audit.jsonl");
    assert!(!lexical.covers(&real_audit), "the lexical form misses it");
    let own = OwnState::canonical(&dirs, base.join("bin/dc"), &canonical_lenient);
    assert!(own.covers(&real_audit));
    assert!(own.covers(&base.join("data/st/disk-cleanup")));
    // A directory that does not exist yet is covered at its canonical spelling too.
    assert!(own.covers(&base.join("data/st/disk-cleanup/new")));
    assert!(!own.covers(&base.join("data/other")));
}

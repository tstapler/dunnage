#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! The catalogue test (UX-41): a copy of the tables of `design/ux.md` 3.5 (markdown backticks removed),
//! compared with what the code renders. A variant without a row fails the test.

use std::collections::HashSet;
use std::path::PathBuf;

use dunnage_core::*;

/// (variant name, reason text, next action)
const SKIP_ROWS: &[(&str, &str, &str)] = &[
    ("Dirty", "dirty working tree", "commit, stash or discard in that worktree"),
    ("Locked", "worktree is locked", "git worktree unlock <path> if intended"),
    ("TooRecent", "younger than --min-age 14d (build dirs: the effective floor, never below 60m)", "lower with --min-age (build dirs cannot go below 60m)"),
    ("UnpushedCommits", "N unpushed commits", "push or delete the branch yourself"),
    ("DetachedHead", "detached HEAD not on any ref", "create a branch first"),
    ("RunningBuild", "in use by a running process", "stop the process, re-run"),
    ("RunStateUnknown", "cannot tell whether a build is running (/proc unreadable)", "run as the owning user"),
    ("IdentityChanged", "changed since scan", "re-run the plan"),
    ("Symlink", "out of scope: symlink", "none (never touched)"),
    ("CrossDevice", "out of scope: cross-device", "none (never touched)"),
    ("MountPoint", "out of scope: mount point", "none (never touched)"),
    ("OutsideRoot", "out of scope: outside root", "none (never touched)"),
    ("ProtectedPath", "out of scope: protected path", "none (never touched)"),
    ("VolumeReferenced", "volume used by container/compose project <x>", "none"),
    ("ImageReferenced", "image used by container <x>", "none"),
    ("DaemonUnavailable", "docker: daemon not reachable (<reason>)", "start Docker yourself; the tool never starts it"),
    ("LibraryUnmounted", "Steam library <path> not mounted", "mount it, re-run"),
    ("SteamRunning", "Steam is running or downloading", "quit Steam, re-run"),
    ("NeedsRoot", "needs root", "sudo <absolute path>/dunnage --apply --kinds <exact-id> (see C5 and 1.5)"),
    ("ToolMissing", "<tool> not installed", "none"),
    ("AlreadyRunning", "btrfs balance already running", "btrfs balance status /"),
    ("Gone", "already gone", "none"),
    ("Aborted", "aborted: ENOSPC / interrupted", "free a little space (the summary prints the hint), then dry run and apply again"),
    ("KeptAsNewest", "kept as newest of N duplicates", "none: the newest copy is kept on purpose (after a partial scan: --rescan)"),
    ("KindNotSelected", "not selected", "add the kind to --kinds"),
    ("RootRestricted", "running as root: your user caches are skipped", "run as your user; use sudo only for cache-apt (the decision is euid 0, whatever HOME holds: sudoers decides whether sudo sets HOME to /root, and that file is root-only, so the tool does not assume it; section 1.5)"),
    ("NonLocalDockerContext", "non-local docker context '<name>'", "docker context use default"),
    ("UnsupportedOnThisOS", "not supported on <OS>", "none"),
    ("UnrecognisedOutput", "unrecognised <tool> output; nothing listed", "update dunnage or report the output shape (the tool never guesses)"),
    ("DockerVersionUntested", "docker <version> is outside the tested range; list-only", "none (removal needs a tested Docker version)"),
    ("LooksLikeDatabase", "volume looks like a database (<signature>, or larger than 1 GiB)", "name it with --volume <name> if you are sure"),
    ("ContentsUnknown", "volume contents not readable, cannot rule out a database", "--volume <name>, or dry run as root: sudo <absolute path>/dunnage --kinds docker-volumes (its plan-id is root's, so apply as root too; section 1.5)"),
    ("MayHoldSaves", "prefix holds files in <dir>, possibly save games", "--steam-saves discard if intended"),
    ("OptInRequired", "list-only: needs its own flag", "add --docker-volumes anonymous or --docker-volumes named"),
    ("InUse", "in use: <process> (pid <N>) has its working directory or an open file under this worktree", "close that session or wait; the tool never stops a process"),
    ("HasIgnoredFiles", "holds ignored files that git would delete with it (<N> files, <size>, for example .env, *.sqlite)", "back them up or remove them yourself, or --worktree-ignored discard if they are disposable"),
    ("NotReproducible", "not reproducible: <detail> (an npm-linked package points outside the project, or no lockfile next to node_modules)", "remove it yourself if you are sure"),
    ("LocalOnlyImage", "image built on this machine (no registry digest): pulling cannot bring it back", "docker rmi <name> by hand if you are sure"),
];

#[test]
fn every_skip_reason_and_delete_error_should_have_a_row_in_ux_s3_5_and_render_it_exactly_When_catalogue_test_runs(
) {
    assert_eq!(SkipReason::ALL.len(), 38);
    let distinct: HashSet<_> = SkipReason::ALL.iter().collect();
    assert_eq!(distinct.len(), 38, "ALL lists a variant twice");
    assert_eq!(SKIP_ROWS.len(), 38);

    for r in SkipReason::ALL {
        let name = format!("{r:?}");
        let row = SKIP_ROWS
            .iter()
            .find(|(n, _, _)| *n == name)
            .unwrap_or_else(|| panic!("{name} has no row in design/ux.md 3.5"));
        assert_eq!(r.reason_text(), row.1, "reason of {name}");
        assert_eq!(r.next_action(), row.2, "next action of {name}");
        assert!(
            !r.next_action().is_empty(),
            "{name} needs a next action or an explicit none"
        );
        let expect = if r == SkipReason::KindNotSelected {
            "RISK"
        } else {
            "SKIP"
        };
        assert_eq!(r.status_word(), expect, "{name}");
    }
    // The wire id of each variant is its kebab-case name.
    assert_eq!(
        serde_json::to_value(SkipReason::KeptAsNewest).unwrap(),
        "kept-as-newest"
    );
    assert_eq!(
        serde_json::to_value(SkipReason::UnsupportedOnThisOS).unwrap(),
        "unsupported-on-this-o-s"
    );

    // DeleteError texts.
    let cases: Vec<(DeleteError, &str)> = vec![
        (
            DeleteError::CommandFailed {
                status: 128,
                message: "fatal: x".into(),
            },
            "command exited 128: fatal: x",
        ),
        (
            DeleteError::CommandFailed {
                status: 128,
                message: String::new(),
            },
            "command exited 128",
        ),
        (DeleteError::Timeout, "command timed out"),
        (DeleteError::Refused, "command refused"),
        (DeleteError::Io(13), "permission denied (EACCES)"),
        (DeleteError::Io(28), "no space left on device"),
        (DeleteError::NoSpace, "no space left on device"),
        (DeleteError::Io(5), "I/O error (errno 5)"),
        (
            DeleteError::Busy,
            "directory changed while deleting (not empty)",
        ),
    ];
    for (e, text) in cases {
        assert_eq!(e.text(), text, "{e:?}");
        assert_eq!(e.skip_reason(), None, "{e:?} is an ERR row");
    }
    let scope = [
        (DeleteError::IdentityMismatch, SkipReason::IdentityChanged),
        (DeleteError::Symlink, SkipReason::Symlink),
        (DeleteError::ProtectedPath, SkipReason::ProtectedPath),
        (DeleteError::OutsideScope, SkipReason::OutsideRoot),
        (DeleteError::CrossDevice, SkipReason::CrossDevice),
    ];
    for (e, r) in scope {
        assert_eq!(e.skip_reason(), Some(r), "{e:?}");
        assert_eq!(e.text(), r.reason_text(), "{e:?}");
    }
}

#[test]
fn every_fatal_error_text_should_end_with_a_next_action_or_an_explicit_none() {
    let state = PathBuf::from("/home/u/.local/state/dunnage");
    let audit = state.join("audit.jsonl");
    let rows: Vec<(FatalError, i32, &str, &str)> = vec![
        (
            FatalError::LockHeld { pid: 4242 },
            2,
            "error: another dunnage run is active (pid 4242). Nothing was deleted. Wait for it to finish ('pgrep -a dunnage' shows it), then run again.",
            "Wait for it to finish ('pgrep -a dunnage' shows it), then run again.",
        ),
        (
            FatalError::AuditUnwritable { path: audit.clone(), errno: 28 },
            2,
            "error: cannot write the audit log /home/u/.local/state/dunnage/audit.jsonl (no space left on device). Nothing was deleted. The log shares the full filesystem: free about 1 MiB by hand (empty the trash, remove one disposable file), then run again.",
            "free about 1 MiB by hand (empty the trash, remove one disposable file), then run again.",
        ),
        (
            FatalError::AuditUnwritable { path: audit.clone(), errno: 122 },
            2,
            "error: cannot write the audit log /home/u/.local/state/dunnage/audit.jsonl (no space left on device). Nothing was deleted. The log shares the full filesystem: free about 1 MiB by hand (empty the trash, remove one disposable file), then run again.",
            "free about 1 MiB by hand (empty the trash, remove one disposable file), then run again.",
        ),
        (
            FatalError::AuditUnwritable { path: audit.clone(), errno: 13 },
            2,
            "error: cannot write the audit log /home/u/.local/state/dunnage/audit.jsonl (permission denied). Nothing was deleted. Fix the ownership or mode of /home/u/.local/state/dunnage, or point XDG_STATE_HOME at a writable directory, then run again.",
            "Fix the ownership or mode of /home/u/.local/state/dunnage, or point XDG_STATE_HOME at a writable directory, then run again.",
        ),
        (
            FatalError::RootsUnreadable { path: "/nope".into(), errno: 2 },
            2,
            "error: root /nope does not exist or is unreadable (no such file or directory). Nothing scanned. Check the spelling of --root (it must be a directory you can read) or drop --root to use the default roots.",
            "Check the spelling of --root (it must be a directory you can read) or drop --root to use the default roots.",
        ),
        (
            FatalError::Starved { unallocated_mib: 600, metadata_headroom_mib: 32 },
            3,
            "error: btrfs unallocated 600 MiB and metadata headroom 32 MiB: deleting could fail with ENOSPC. Nothing was deleted. Free ~1 GiB by hand (empty trash, remove one known-disposable file), then re-run.",
            "Free ~1 GiB by hand (empty trash, remove one known-disposable file), then re-run.",
        ),
        (
            FatalError::StateUnreadable { path: "/".into(), reason: "permission denied".into() },
            74,
            "error: cannot read filesystem state for /: permission denied. Check that the path is mounted ('df -- /') or run as a user that can read /sys/fs/btrfs.",
            "run as a user that can read /sys/fs/btrfs.",
        ),
    ];
    for (e, code, text, action) in rows {
        assert_eq!(e.text(), text, "{e:?}");
        assert_eq!(e.to_string(), text);
        assert_eq!(e.exit_code(), code, "{e:?}");
        assert!(
            e.text().ends_with(action),
            "{e:?} must end with its next action"
        );
        assert!(e.text().starts_with("error: "));
    }
}

#[test]
fn catalogue_test_should_cover_warning_texts_and_the_four_iteration_3_skip_reasons() {
    for r in [
        SkipReason::InUse,
        SkipReason::HasIgnoredFiles,
        SkipReason::NotReproducible,
        SkipReason::LocalOnlyImage,
    ] {
        assert!(!r.next_action().is_empty(), "{r:?}");
        assert!(SKIP_ROWS.iter().any(|(n, _, _)| *n == format!("{r:?}")));
    }
    let dir = PathBuf::from("/s");
    let audit = PathBuf::from("/s/audit.jsonl");
    // (warning, exact text, the text that is its next action, or "" for a pure note)
    let rows: Vec<(Warning, &str, &str)> = vec![
        (
            Warning::PreviousRunUnfinished { run_id: "r9".into(), audit_path: audit.clone() },
            "warning: previous run r9 did not finish (killed or power loss); /s/audit.jsonl may show a partly removed item. Check it with 'dunnage audit verify' or jq 'select(.run_id==\"r9\")' /s/audit.jsonl. A partly removed build dir: dry run again; a partly removed worktree: 'git -C <repo> worktree list', then 'git -C <repo> worktree prune' once its directory is gone.",
            "'dunnage audit verify'",
        ),
        (
            Warning::PlanNotSaved { errno: 28, path: "/s/last_plan.json".into() },
            "warning: could not save the plan (no space left on device on /s/last_plan.json). Its plan-id cannot be used; see the NOT usable block.",
            "see the NOT usable block.",
        ),
        (
            Warning::LastPlanUnreadable { path: "/s/last_plan.json".into() },
            "warning: last_plan (/s/last_plan.json) is unreadable or written by a newer version; treating it as no dry run. Dry run again.",
            "Dry run again.",
        ),
        (
            Warning::PlanDuringApply { pid: 4242 },
            "note: another dunnage apply is running (pid 4242); sizes and items below are in flux and a plan-id from this report may be refused as changed once it finishes.",
            "",
        ),
        (
            Warning::ReservesCreated { state_dir: dir },
            "note: created 2 reserve files of 1 MiB in /s (they let the tool save a plan and open its audit log when the disk is full)",
            "",
        ),
        (
            Warning::PlanLockBusy,
            "warning: could not save the plan: last_plan.lock busy for 2 s. Dry run again in a moment.",
            "Dry run again in a moment.",
        ),
    ];
    for (w, text, action) in rows {
        assert_eq!(w.text(), text, "{w:?}");
        if text.starts_with("warning: ") {
            assert!(!action.is_empty(), "{w:?}: a warning needs a next action");
            assert!(text.contains(action), "{w:?}");
        }
    }
}

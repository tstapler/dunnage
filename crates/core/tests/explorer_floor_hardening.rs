//! Hardening of the explorer's hard floor (0.3.1). Written from the review findings and the floor list in
//! SECURITY.md, before the fixes. Same author as the implementation, NOT an independent one.
//!
//! Everything runs on tables: a fake filesystem whose `lstat` answers from a map and a fake `canon`, so a
//! second path to the same directory (symlinked `~/.ssh`, a bind mount, a btrfs top-level mount) is just two
//! paths with one identity.

#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use dunnage_core::delete_types::{DirEntryInfo, FileKind, StatInfo};
use dunnage_core::explore::mark_check::{
    mark_check, MarkCtx, MarkEnv, MarkFacts, MarkMount, MountTable, OwnState, SymlinkFact,
};
use dunnage_core::explore::marks::Marks;
use dunnage_core::explorer_floor::{explorer_floor, FloorReason};
use dunnage_core::explorer_plan::{judge_fresh, FreshCheck};
use dunnage_core::guard::{decide_path_common, Facts, GuardDecision};
use dunnage_core::ports::Fs;
use dunnage_core::roots::RootSet;
use dunnage_core::{Dirs, Identity, Kind, RootDev, SkipReason};

const HOME: &str = "/home/u";

fn floor(p: &str) -> Option<FloorReason> {
    explorer_floor(Path::new(p), Path::new(HOME))
}

// ------------------------------------------------------------------ finding 2: other accounts

#[test]
fn safety_explorer_floor_should_refuse_everything_inside_another_accounts_home() {
    for p in [
        "/home/other/.ssh/x",
        "/home/other/code",
        "/home/other/code/app",
        "/Users/o/.ssh/x",
        "/Users/o/Documents/x",
        "/var/home/u/.ssh/x",
        "/var/home/u/code/app",
        "/root/.ssh/x",
        "/root/x",
        "/root/a/b/c",
    ] {
        assert!(floor(p).is_some(), "{p} must be on the floor");
    }
}

#[test]
fn explorer_floor_should_still_allow_the_real_home_whatever_its_spelling() {
    assert_eq!(floor("/home/u/code/app"), None);
    let root = |p: &str| explorer_floor(Path::new(p), Path::new("/root"));
    assert_eq!(root("/root/code/app"), None);
    assert!(root("/root/.ssh/x").is_some());
    let mac = |p: &str| explorer_floor(Path::new(p), Path::new("/Users/me"));
    assert_eq!(mac("/Users/me/code/app"), None);
    assert!(mac("/Users/other/code/app").is_some());
    let silver = |p: &str| explorer_floor(Path::new(p), Path::new("/var/home/u"));
    assert_eq!(silver("/var/home/u/code/app"), None);
    assert!(silver("/var/home/other/code/app").is_some());
}

// ------------------------------------------------------------------ finding 3: the list

#[test]
fn safety_explorer_floor_should_refuse_the_credential_and_browser_trees_what_holds_them_and_case_variants(
) {
    for rel in [
        ".local/share/keyrings",
        ".password-store",
        ".aws",
        ".kube",
        ".config/gh",
        ".config/gcloud",
        ".config/Bitwarden",
        ".config/sops",
        ".config/gnupg",
        ".local/share/gnupg",
        ".mozilla",
        ".config/google-chrome",
        ".config/chromium",
        ".config/BraveSoftware",
        "snap/1password",
        ".var/app/com.1password.1Password",
        "Library/Keychains",
    ] {
        let inner = format!("{HOME}/{rel}/x/y");
        assert_eq!(floor(&inner), Some(FloorReason::Secrets), "{inner}");
        let exact = format!("{HOME}/{rel}");
        assert!(floor(&exact).is_some(), "{exact}");
        let upper = format!("{HOME}/{}/x/y", rel.to_uppercase());
        assert!(floor(&upper).is_some(), "{upper}");
    }
    // a folder that holds a secret directory
    assert_eq!(floor("/home/u/.local/share"), Some(FloorReason::Secrets));
    // neighbours stay deletable
    for p in [
        "/home/u/.config/gh-notes/x",
        "/home/u/.local/share/Trash/x",
        "/home/u/.awsx/x",
        "/home/u/.mozillax/x",
        "/home/u/.config/google-chrome-beta-notes/x",
    ] {
        assert_eq!(floor(p), None, "{p}");
    }
}

#[test]
fn safety_explorer_floor_should_refuse_system_application_trees_and_agent_sockets() {
    for p in [
        "/opt/thing",
        "/opt/google/chrome",
        "/nix/store/abc",
        "/snap/core/1",
        "/efi/EFI/x",
        "/lost+found/x",
        "/Applications/Foo.app/x",
        "/tmp/ssh-AbCdEf/agent.12",
        "/private/tmp/ssh-AbCdEf/agent.12",
        "/tmp/.X11-unix/X0",
    ] {
        assert!(floor(p).is_some(), "{p} must be on the floor");
    }
    for p in [
        "/tmp/scratch",
        "/tmp/ssh/x",
        "/tmp/sshfs-mount/x",
        "/srv/data/old",
        "/mnt/disk/x",
    ] {
        assert_eq!(floor(p), None, "{p}");
    }
}

// ------------------------------------------------------------------ fakes

fn id(ino: u64) -> Identity {
    Identity { dev: 1, ino }
}

fn stat(ino: u64) -> StatInfo {
    StatInfo {
        ident: id(ino),
        kind: FileKind::Dir,
        mode: 0o755,
        nlink: 2,
        blocks: 8,
        size: 4096,
        mtime_secs: 1_700_000_000,
        mtime_nsec: 0,
        mnt_id: Some(1),
        btime: None,
    }
}

/// `lstat` answers from a table; every path not in it does not exist.
#[derive(Default)]
struct TableFs(HashMap<PathBuf, StatInfo>);

impl TableFs {
    fn with(mut self, path: &str, ino: u64) -> TableFs {
        self.0.insert(PathBuf::from(path), stat(ino));
        self
    }
}

impl Fs for TableFs {
    fn lstat(&self, path: &Path) -> io::Result<StatInfo> {
        self.0
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
    fn read_dir(&self, _: &Path) -> io::Result<Vec<DirEntryInfo>> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
    fn read_to_string(&self, _: &Path) -> io::Result<String> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

/// The machine: `/` (1), `/home` (2), `/home/u` (3), the secret directories of `/home/u` (10..), the tool's
/// state directory (20). The same directories again under a btrfs top-level mount at `/mnt/top/@home`.
fn world() -> TableFs {
    TableFs::default()
        .with("/", 1)
        .with("/home", 2)
        .with("/home/u", 3)
        .with("/home/u/.ssh", 10)
        .with("/home/u/.local", 11)
        .with("/home/u/.local/share", 12)
        .with("/home/u/.local/share/keyrings", 13)
        .with("/home/u/.local/state", 14)
        .with("/home/u/.local/state/dunnage", 20)
        .with("/mnt", 30)
        .with("/mnt/top", 31)
        .with("/mnt/top/@home", 2)
        .with("/mnt/top/@home/u", 3)
        .with("/mnt/top/@home/u/.ssh", 10)
        .with("/mnt/top/@home/u/.ssh/x", 40)
        .with("/mnt/top/@home/u/.local", 11)
        .with("/mnt/top/@home/u/.local/share", 12)
        .with("/mnt/top/@home/u/.local/share/keyrings", 13)
        .with("/mnt/top/@home/u/.local/share/keyrings/old", 41)
        .with("/mnt/top/@home/u/.local/state", 14)
        .with("/mnt/top/@home/u/.local/state/dunnage", 20)
        .with("/mnt/top/@home/u/.local/state/dunnage/sub", 42)
        .with("/mnt/top/@home/u/code", 50)
        .with("/mnt/top/@home/u/code/app", 51)
        .with("/mnt/top/@home/u/code/app/dist", 52)
}

fn canon(p: &Path) -> io::Result<(PathBuf, Identity)> {
    world()
        .lstat(p)
        .map(|s| (p.to_path_buf(), s.ident))
        .map_err(|_| io::Error::from(io::ErrorKind::NotFound))
}

fn dirs(config: &str) -> Dirs {
    Dirs {
        home: HOME.into(),
        state: "/home/u/.local/state".into(),
        cache: "/home/u/.cache".into(),
        config: config.into(),
    }
}

fn roots_with(config: &str) -> RootSet {
    RootSet::resolve(&dirs(config), &[], Some(&[]), Path::new("/"), &canon).expect("roots")
}

fn own_state() -> OwnState {
    OwnState::new(&dirs("/home/u/.config"), "/usr/bin/dunnage".into())
}

fn judge(path: &str, fs: &TableFs, roots: &RootSet) -> Result<(), SkipReason> {
    let st = fs.lstat(Path::new(path)).expect("path in the table");
    let (mounts, points) = (
        MountTable::new(vec![MarkMount {
            point: "/".into(),
            source: "/dev/sda1".into(),
        }]),
        vec![PathBuf::from("/")],
    );
    let own = own_state();
    let fc = FreshCheck {
        roots,
        own_state: &own,
        fs,
    };
    judge_fresh(Path::new(path), &st, &mounts, &points, &fc)
        .allowed
        .map(|_| ())
}

// ------------------------------------------------------------------ finding 1: second paths

#[test]
fn judge_fresh_should_allow_an_ordinary_directory_on_the_second_mount_path() {
    let fs = world();
    assert_eq!(
        judge(
            "/mnt/top/@home/u/code/app/dist",
            &fs,
            &roots_with("/home/u/.config")
        ),
        Ok(())
    );
}

#[test]
fn safety_judge_fresh_should_refuse_a_secret_directory_reached_by_a_second_path() {
    let fs = world();
    let roots = roots_with("/home/u/.config");
    assert_eq!(
        judge("/mnt/top/@home/u/.ssh/x", &fs, &roots),
        Err(SkipReason::ProtectedPath)
    );
    assert_eq!(
        judge("/mnt/top/@home/u/.local/share/keyrings/old", &fs, &roots),
        Err(SkipReason::ProtectedPath)
    );
    assert_eq!(
        judge("/mnt/top/@home/u/.ssh", &fs, &roots),
        Err(SkipReason::ProtectedPath)
    );
}

#[test]
fn safety_judge_fresh_should_refuse_a_folder_that_holds_a_secret_directory_by_identity() {
    let fs = world();
    let roots = roots_with("/home/u/.config");
    assert_eq!(
        judge("/mnt/top/@home/u/.local/share", &fs, &roots),
        Err(SkipReason::ProtectedPath)
    );
}

#[test]
fn safety_judge_fresh_should_refuse_the_tools_own_state_reached_by_a_second_path() {
    let fs = world();
    let roots = roots_with("/home/u/.config");
    assert_eq!(
        judge("/mnt/top/@home/u/.local/state/dunnage/sub", &fs, &roots),
        Err(SkipReason::ProtectedPath)
    );
    assert_eq!(
        judge("/mnt/top/@home/u/.local/state/dunnage", &fs, &roots),
        Err(SkipReason::ProtectedPath)
    );
}

// ------------------------------------------------------------------ finding 3: XDG_CONFIG_HOME

fn facts(path: &str, ino: u64) -> Facts {
    Facts {
        kind: Kind::ExplorerSelection,
        path: path.into(),
        ident: id(ino),
        is_symlink: false,
        root_dev: RootDev(1),
        is_mount_point: false,
    }
}

#[test]
fn safety_guard_should_refuse_a_secret_below_a_moved_xdg_config_home() {
    let roots = roots_with("/home/u/cfg");
    for p in [
        "/home/u/cfg/gh/hosts",
        "/home/u/cfg/gcloud/x/y",
        "/home/u/cfg/1Password/x",
    ] {
        assert_eq!(
            decide_path_common(&facts(p, 90), &roots).skipped(),
            Some(SkipReason::ProtectedPath),
            "{p}"
        );
    }
    assert!(matches!(
        decide_path_common(&facts("/home/u/cfg/other/x", 91), &roots),
        GuardDecision::Allow(_)
    ));
}

trait Skipped {
    fn skipped(&self) -> Option<SkipReason>;
}
impl Skipped for GuardDecision {
    fn skipped(&self) -> Option<SkipReason> {
        match self {
            GuardDecision::Skip(r) => Some(*r),
            GuardDecision::Allow(_) => None,
        }
    }
}

// ------------------------------------------------------------------ finding 6: .git at the engine

#[test]
fn safety_guard_should_refuse_a_git_component_below_any_path_for_an_explorer_mark() {
    let roots = roots_with("/home/u/.config");
    for p in [
        "/srv/data/repo/.git/hooks",
        "/home/u/code/app/.git/hooks",
        "/mnt/top/@home/u/code/app/.git/hooks",
    ] {
        assert_eq!(
            decide_path_common(&facts(p, 70), &roots).skipped(),
            Some(SkipReason::ProtectedPath),
            "{p}"
        );
        assert_eq!(floor(p), Some(FloorReason::Git), "{p}");
    }
}

// ------------------------------------------------------------------ finding 7: fail closed

struct Facts7 {
    dev_of_root: Option<u64>,
}

impl MarkFacts for Facts7 {
    fn symlink_in(&self, _: &Path) -> Option<SymlinkFact> {
        None
    }
    fn dev_of(&self, p: &Path) -> Option<u64> {
        if p == Path::new("/") {
            self.dev_of_root
        } else {
            Some(1)
        }
    }
    fn is_dir(&self, _: &Path) -> Option<bool> {
        Some(true)
    }
    fn is_boundary(&self, _: &Path) -> bool {
        false
    }
    fn scanning(&self, _: &Path) -> bool {
        false
    }
}

fn mark(root_dev: Option<u64>) -> bool {
    let mounts = MountTable::new(vec![MarkMount {
        point: "/".into(),
        source: "/dev/sda1".into(),
    }]);
    let own = own_state();
    let f = Facts7 {
        dev_of_root: root_dev,
    };
    let env = MarkEnv {
        roots: &[],
        home: Path::new(HOME),
        mounts: &mounts,
        own_state: &own,
        facts: &f,
        tty: false,
    };
    let marks = Marks::new();
    mark_check(
        Path::new("/srv/data/old"),
        &MarkCtx {
            env: &env,
            marks: &marks,
        },
    )
    .is_ok()
}

#[test]
fn mark_check_should_accept_a_directory_whose_anchor_device_is_known_and_equal() {
    assert!(mark(Some(1)));
}

#[test]
fn safety_mark_check_should_refuse_when_the_device_of_the_starting_directory_is_unreadable() {
    assert!(!mark(None));
}

#[test]
fn safety_judge_fresh_should_refuse_when_the_starting_directory_cannot_be_read_now() {
    let with_root = world()
        .with("/srv", 60)
        .with("/srv/data", 61)
        .with("/srv/data/old", 62);
    let roots = roots_with("/home/u/.config");
    assert_eq!(judge("/srv/data/old", &with_root, &roots), Ok(()));
    // `/` is in the mount table but its lstat fails: the item's own device must not stand in for it.
    let mut no_root = with_root;
    no_root.0.remove(Path::new("/"));
    assert!(judge("/srv/data/old", &no_root, &roots).is_err());
}

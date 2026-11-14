//! The hard floor of the explorer's marks: paths `dunnage explore` never marks or deletes, wherever they are.
//!
//! The cleaner kinds stay inside their narrow roots. An explorer mark may name any directory the owner reviewed,
//! so the guard is a list of what is refused instead of a list of what is allowed. This module is that list. It
//! is pure and lexical: it reads no file, so the screen (mark rule M1), the planner, the engine's
//! `DeletePolicy::validate` and `audit verify` all call this one function and cannot disagree. Symlinks, other
//! filesystems, mount points and the tool's own state are separate rules (M3 to M5) because they need the
//! filesystem.
//!
//! Matching is ASCII case-insensitive: macOS volumes are case-insensitive by default, so `~/.SSH` is `~/.ssh`.
//! A refusal that is too broad on a case-sensitive volume costs a rename; a miss costs the key.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// Top-level directories that hold the operating system, whatever lies below them.
const SYSTEM_TOPS: [&str; 15] = [
    "usr", "etc", "boot", "bin", "sbin", "lib", "lib32", "lib64", "libx32", "dev", "proc", "sys",
    "run", "System", "Library",
];

/// Directories of secrets, relative to `$HOME`. Each one, everything below it and everything that holds it is
/// refused: the 1Password desktop app and CLI state, and the agent socket directories on Linux and macOS.
const SECRET_DIRS: [&[&str]; 8] = [
    &[".ssh"],
    &[".gnupg"],
    &[".1password"],
    &[".config", "1Password"],
    &[".config", "op"],
    &["Library", "Group Containers", "2BUA8C4S2C.com.1password"],
    &["Library", "Application Support", "1Password"],
    &["Library", "Containers", "com.1password.1password"],
];

/// Why a path is on the floor. [`FloorReason::text`] is the line the screen and `audit verify` print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloorReason {
    /// Not absolute, not UTF-8, or has `.`, `..`, an empty component or a trailing `/`.
    NotClean,
    /// `/`.
    FilesystemRoot,
    /// A direct child of `/` (`/home`, `/var`, `/tmp`, `/srv`, `/mnt`, `/opt`, `/Users`, ...).
    TopLevel,
    /// Inside, or equal to, a system directory (`/usr`, `/etc`, `/boot`, `/bin`, `/sbin`, `/lib*`, `/dev`,
    /// `/proc`, `/sys`, `/run`, `/System`, `/Library`).
    System,
    /// Inside `/var` but not below `/var/tmp` or `/var/cache`, or one of those two themselves.
    VarState,
    /// Another account's home directory (`/home/<user>`, `/Users/<user>`, `/var/home/<user>`).
    AccountHome,
    /// `$HOME` itself.
    Home,
    /// An ancestor of `$HOME`.
    HomeAncestor,
    /// Fewer than two components below `$HOME` (rule F of the delete engine).
    ShallowInHome,
    /// A key or credential directory, or a directory that holds one.
    Secrets,
    /// A `.git` component anywhere in the path.
    Git,
}

impl FloorReason {
    /// What the owner reads: the reason, without the path.
    pub fn text(self) -> &'static str {
        match self {
            FloorReason::NotClean => {
                "the path is not absolute, clean UTF-8 (no '.', '..', empty or trailing-slash parts)"
            }
            FloorReason::FilesystemRoot => "the filesystem root is never deleted",
            FloorReason::TopLevel => "a top-level directory of the filesystem is never deleted",
            FloorReason::System => "a system directory is never deleted from here",
            FloorReason::VarState => {
                "only folders below /var/tmp and /var/cache can be deleted from /var"
            }
            FloorReason::AccountHome => "a user's home directory is never deleted",
            FloorReason::Home => "your home directory is never deleted",
            FloorReason::HomeAncestor => "a folder that holds your home directory is never deleted",
            FloorReason::ShallowInHome => {
                "a folder directly inside your home directory is never deleted; mark a folder inside it"
            }
            FloorReason::Secrets => {
                "keys and credentials (.ssh, .gnupg, 1Password and its agent socket) are never deleted"
            }
            FloorReason::Git => "a .git directory is never entered or deleted",
        }
    }
}

fn eq_ci(a: &OsStr, b: &str) -> bool {
    a.as_bytes().eq_ignore_ascii_case(b.as_bytes())
}

/// The components of a clean absolute UTF-8 path, or `None`.
fn parts(path: &Path) -> Option<Vec<&OsStr>> {
    let text = path.to_str()?;
    let mut it = path.components();
    if it.next() != Some(Component::RootDir) {
        return None;
    }
    let mut out = Vec::new();
    for c in it {
        match c {
            Component::Normal(n) => out.push(n),
            _ => return None,
        }
    }
    // `Path` hides `a//b`, `a/./b` and a trailing `/`: compare the spelling too.
    let joined: Vec<&str> = out.iter().filter_map(|n| n.to_str()).collect();
    (format!("/{}", joined.join("/")) == text).then_some(out)
}

/// `Some(why)` when `path` can never be an explorer mark; `None` when only the filesystem rules can still
/// refuse it. `home` is canonical `$HOME` (`RootSet::home`).
pub fn explorer_floor(path: &Path, home: &Path) -> Option<FloorReason> {
    let Some(mut p) = parts(path) else {
        return Some(FloorReason::NotClean);
    };
    if p.is_empty() {
        return Some(FloorReason::FilesystemRoot);
    }
    if p.iter().any(|c| eq_ci(c, ".git")) {
        return Some(FloorReason::Git);
    }
    // macOS spells `/etc`, `/var` and `/tmp` as `/private/...`.
    if p.len() >= 2
        && eq_ci(p[0], "private")
        && ["etc", "var", "tmp"].iter().any(|n| eq_ci(p[1], n))
    {
        p.remove(0);
    }
    let in_home = home_rules(path, home);
    if matches!(in_home, Some(FloorReason::Home | FloorReason::HomeAncestor)) {
        return in_home;
    }
    if p.len() == 1 {
        return Some(FloorReason::TopLevel);
    }
    if SYSTEM_TOPS.iter().any(|n| eq_ci(p[0], n)) {
        return Some(FloorReason::System);
    }
    if eq_ci(p[0], "var") {
        let var_home = p.len() >= 3 && eq_ci(p[1], "home");
        if var_home && p.len() == 3 {
            return Some(FloorReason::AccountHome);
        }
        let scratch = p.len() >= 3 && (eq_ci(p[1], "tmp") || eq_ci(p[1], "cache"));
        if !(scratch || var_home) {
            return Some(FloorReason::VarState);
        }
    }
    if p.len() == 2 && (eq_ci(p[0], "home") || eq_ci(p[0], "Users")) {
        return Some(FloorReason::AccountHome);
    }
    in_home
}

fn same_prefix(a: &[&OsStr], b: &[&OsStr]) -> bool {
    a.len() <= b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.as_bytes().eq_ignore_ascii_case(y.as_bytes()))
}

fn home_rules(path: &Path, home: &Path) -> Option<FloorReason> {
    let (Some(h), Some(p)) = (parts(home), parts(path)) else {
        return None;
    };
    if p.len() == h.len() && same_prefix(&h, &p) {
        return Some(FloorReason::Home);
    }
    if same_prefix(&p, &h) {
        return Some(FloorReason::HomeAncestor);
    }
    if !same_prefix(&h, &p) {
        return None;
    }
    let rel = &p[h.len()..];
    if rel.len() < 2 {
        return Some(FloorReason::ShallowInHome);
    }
    let secret = SECRET_DIRS.iter().any(|s| {
        let n = s.len().min(rel.len());
        rel[..n].iter().zip(&s[..n]).all(|(a, b)| eq_ci(a, b))
    });
    secret.then_some(FloorReason::Secrets)
}

/// The directory a delete opens first and walks down from: the longest scan root that holds `path`, else the
/// longest mount point that does. Neither is `path` itself. `None` when nothing holds it (no mount table):
/// the caller refuses.
///
/// Walking down from a mount point keeps the whole path on one filesystem; walking down from a scan root is
/// what the cleaner kinds and the explorer have always done. The delete engine still compares every directory
/// it opens with the anchor's device and mount id.
pub fn explorer_anchor<'a>(
    path: &Path,
    scan_roots: &'a [PathBuf],
    mount_points: impl IntoIterator<Item = &'a Path>,
) -> Option<&'a Path> {
    let holds = |r: &Path| {
        path.strip_prefix(r)
            .is_ok_and(|rel| !rel.as_os_str().is_empty())
    };
    let longest = |it: &mut dyn Iterator<Item = &'a Path>| {
        it.filter(|r| holds(r))
            .max_by_key(|r| r.components().count())
    };
    longest(&mut scan_roots.iter().map(PathBuf::as_path))
        .or_else(|| longest(&mut mount_points.into_iter()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/u";

    fn floor(p: &str) -> Option<FloorReason> {
        explorer_floor(Path::new(p), Path::new(HOME))
    }

    #[test]
    fn explorer_floor_should_allow_ordinary_folders_anywhere_a_user_may_delete() {
        for p in [
            "/home/u/code/app",
            "/home/u/Downloads/old",
            "/home/u/.cache/thing",
            "/home/u/a/b/c/d",
            "/tmp/scratch",
            "/srv/data/old",
            "/opt/thing",
            "/mnt/disk/stuff",
            "/var/tmp/x",
            "/var/cache/x",
            "/var/tmp/x/y",
            "/media/u/usb/dir",
            "/private/tmp/x",
            "/Volumes/Data/dir",
        ] {
            assert_eq!(floor(p), None, "{p}");
        }
    }

    #[test]
    fn explorer_floor_should_refuse_the_root_and_every_top_level_directory() {
        assert_eq!(floor("/"), Some(FloorReason::FilesystemRoot));
        for p in [
            "/var", "/tmp", "/srv", "/mnt", "/opt", "/Users", "/root",
        ] {
            assert_eq!(floor(p), Some(FloorReason::TopLevel), "{p}");
        }
    }

    #[test]
    fn explorer_floor_should_refuse_system_trees_at_any_depth() {
        for p in [
            "/usr/local/foo",
            "/etc/foo",
            "/boot/efi/x",
            "/bin/x",
            "/sbin/x",
            "/lib/modules/x",
            "/lib64/x",
            "/dev/shm/x",
            "/proc/1/x",
            "/sys/fs/x",
            "/run/user/1000",
            "/System/Library/x",
            "/Library/Caches/x",
            "/USR/local/x",
        ] {
            assert_eq!(floor(p), Some(FloorReason::System), "{p}");
        }
    }

    #[test]
    fn explorer_floor_should_allow_only_var_tmp_and_var_cache_below_var() {
        assert_eq!(floor("/var/lib/x"), Some(FloorReason::VarState));
        assert_eq!(floor("/var/tmp"), Some(FloorReason::VarState));
        assert_eq!(floor("/var/cache"), Some(FloorReason::VarState));
        assert_eq!(floor("/var/tmp/x"), None);
        assert_eq!(floor("/private/var/lib/x"), Some(FloorReason::VarState));
    }

    #[test]
    fn explorer_floor_should_refuse_home_its_ancestors_and_other_accounts_homes() {
        assert_eq!(floor("/home/u"), Some(FloorReason::Home));
        assert_eq!(floor("/home"), Some(FloorReason::HomeAncestor));
        assert_eq!(floor("/home/other"), Some(FloorReason::AccountHome));
        assert_eq!(floor("/Users/other"), Some(FloorReason::AccountHome));
        assert_eq!(floor("/var/home/other"), Some(FloorReason::AccountHome));
        let silver = |p: &str| explorer_floor(Path::new(p), Path::new("/var/home/u"));
        assert_eq!(silver("/var/home/u"), Some(FloorReason::Home));
        assert_eq!(silver("/var/home/u/code/app"), None);
        assert_eq!(silver("/var/home"), Some(FloorReason::HomeAncestor));
        let deep = |p: &str| explorer_floor(Path::new(p), Path::new("/a/b/c/u"));
        assert_eq!(deep("/a/b/c"), Some(FloorReason::HomeAncestor));
        assert_eq!(deep("/a/b/c/u"), Some(FloorReason::Home));
        assert_eq!(deep("/a/b"), Some(FloorReason::HomeAncestor));
    }

    #[test]
    fn explorer_floor_should_refuse_a_folder_directly_inside_home() {
        assert_eq!(floor("/home/u/Downloads"), Some(FloorReason::ShallowInHome));
        assert_eq!(floor("/home/u/.config"), Some(FloorReason::ShallowInHome));
    }

    #[test]
    fn explorer_floor_should_refuse_secrets_what_holds_them_and_case_variants() {
        for p in [
            "/home/u/.ssh/x",
            "/home/u/.gnupg/private-keys-v1.d",
            "/home/u/.config/1Password",
            "/home/u/.config/1Password/x/y",
            "/home/u/.config/op/x",
            "/home/u/.1password/x",
            "/home/u/.SSH/x",
            "/home/u/.config/1PASSWORD/x",
            "/home/u/Library/Group Containers/2BUA8C4S2C.com.1password/t",
            "/home/u/Library/Group Containers/2BUA8C4S2C.com.1password",
            "/home/u/Library/Group Containers",
            "/home/u/Library/Application Support/1Password/x",
        ] {
            assert_eq!(floor(p), Some(FloorReason::Secrets), "{p}");
        }
        assert_eq!(floor("/home/u/.sshx/x"), None);
        assert_eq!(floor("/home/u/.config/1Passwordish/x"), None);
        assert_eq!(floor("/home/u/Library/Caches/x"), None);
    }

    #[test]
    fn explorer_floor_should_refuse_a_git_component_anywhere_even_outside_home() {
        for p in [
            "/srv/r/.git",
            "/srv/r/.git/hooks",
            "/home/u/c/a/.GIT/x",
            "/tmp/a/.git",
        ] {
            assert_eq!(floor(p), Some(FloorReason::Git), "{p}");
        }
        assert_eq!(floor("/srv/r/.gitx"), None);
    }

    #[test]
    fn explorer_floor_should_refuse_a_path_that_is_not_lexically_clean() {
        for p in [
            "relative/dir",
            "/srv/../etc/x",
            "/srv/./x",
            "/srv//x",
            "/srv/x/",
            "",
            "/srv/a/../../etc",
        ] {
            assert_eq!(floor(p), Some(FloorReason::NotClean), "{p:?}");
        }
    }

    #[test]
    fn explorer_floor_should_refuse_a_name_that_is_not_utf8() {
        let p = Path::new(OsStr::from_bytes(b"/srv/data/\xff\xfe"));
        assert_eq!(
            explorer_floor(p, Path::new(HOME)),
            Some(FloorReason::NotClean)
        );
    }

    #[test]
    fn explorer_anchor_should_prefer_the_longest_scan_root_then_the_longest_mount_point() {
        let roots = vec![PathBuf::from("/home/u/code"), PathBuf::from("/srv/work")];
        let mounts = [
            Path::new("/"),
            Path::new("/srv"),
            Path::new("/home/u/code/usb"),
        ];
        let a = |p: &str| explorer_anchor(Path::new(p), &roots, mounts).map(Path::to_path_buf);
        assert_eq!(a("/home/u/code/app"), Some("/home/u/code".into()));
        assert_eq!(a("/home/u/code/usb/x"), Some("/home/u/code".into()));
        assert_eq!(a("/srv/data/x"), Some("/srv".into()));
        assert_eq!(a("/opt/x/y"), Some("/".into()));
        assert_eq!(a("/srv"), Some("/".into()));
        assert_eq!(explorer_anchor(Path::new("/opt/x"), &roots, []), None);
    }
}

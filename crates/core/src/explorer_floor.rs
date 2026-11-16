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

use crate::roots::lexically_clean;

/// Top-level directories that hold the operating system or installed applications, whatever lies below them.
/// `/opt`, `/nix`, `/snap`, `/efi`, `/lost+found` and `/Applications` hold programs, not the owner's work.
const SYSTEM_TOPS: [&str; 21] = [
    "usr",
    "etc",
    "boot",
    "bin",
    "sbin",
    "lib",
    "lib32",
    "lib64",
    "libx32",
    "dev",
    "proc",
    "sys",
    "run",
    "System",
    "Library",
    "opt",
    "nix",
    "snap",
    "efi",
    "lost+found",
    "Applications",
];

/// Directories of secrets, relative to `$HOME`. Each one, everything below it and everything that holds it is
/// refused: keys, keyrings, cloud and password-manager credentials, browser profiles (cookies and saved
/// logins), and the 1Password app state and agent sockets on Linux (also snap and flatpak) and macOS.
/// SECURITY.md says why each is on the list.
const SECRET_DIRS: [&[&str]; 32] = [
    &[".ssh"],
    &[".gnupg"],
    &[".1password"],
    &[".password-store"],
    &[".aws"],
    &[".kube"],
    &[".mozilla"],
    &[".config", "1Password"],
    &[".config", "op"],
    &[".config", "gh"],
    &[".config", "gcloud"],
    &[".config", "Bitwarden"],
    &[".config", "sops"],
    &[".config", "gnupg"],
    &[".config", "google-chrome"],
    &[".config", "chromium"],
    &[".config", "BraveSoftware"],
    &[".local", "share", "keyrings"],
    &[".local", "share", "gnupg"],
    &["snap", "1password"],
    &[".var", "app", "com.1password.1Password"],
    &["Library", "Group Containers", "2BUA8C4S2C.com.1password"],
    &["Library", "Application Support", "1Password"],
    &["Library", "Containers", "com.1password.1password"],
    &["Library", "Keychains"],
    &[".local", "share", "kwalletd"],
    &[".docker"],
    &[".azure"],
    &[".thunderbird"],
    &[".config", "vivaldi"],
    &[".config", "keepassxc"],
    &[".config", "rclone"],
];

/// The secret directories that live below the XDG config home (`~/.config` unless `XDG_CONFIG_HOME` moves it).
const CONFIG_SECRETS: [&str; 10] = [
    "1Password",
    "op",
    "gh",
    "gcloud",
    "Bitwarden",
    "sops",
    "gnupg",
    "google-chrome",
    "chromium",
    "BraveSoftware",
];

/// The secret directories that live below the XDG data home (`~/.local/share` unless `XDG_DATA_HOME` moves it).
const DATA_SECRETS: [&str; 2] = ["keyrings", "gnupg"];

/// Every secret directory below `$HOME` by its default spelling, plus the ones below `config_home` and
/// `data_home` when those are given and lie strictly below `home` (a moved `XDG_CONFIG_HOME` or
/// `XDG_DATA_HOME`). Absolute and lexical: nothing is read.
pub fn secret_dirs(
    home: &Path,
    config_home: Option<&Path>,
    data_home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = SECRET_DIRS
        .iter()
        .map(|rel| rel.iter().fold(home.to_path_buf(), |p, c| p.join(c)))
        .collect();
    let below_home = |p: &Path| p.starts_with(home) && p != home && lexically_clean(p);
    if let Some(c) = config_home.filter(|c| below_home(c)) {
        out.extend(CONFIG_SECRETS.iter().map(|n| c.join(n)));
    }
    if let Some(d) = data_home.filter(|d| below_home(d)) {
        out.extend(DATA_SECRETS.iter().map(|n| d.join(n)));
    }
    out.sort();
    out.dedup();
    out
}

/// `secret` is `path`, below it, or holds it, comparing names ASCII case-insensitively.
pub fn touches_secret(path: &Path, secret: &Path) -> bool {
    let (Some(p), Some(s)) = (parts(path), parts(secret)) else {
        return false;
    };
    let n = p.len().min(s.len());
    p[..n]
        .iter()
        .zip(&s[..n])
        .all(|(a, b)| a.as_bytes().eq_ignore_ascii_case(b.as_bytes()))
}

/// Why a path is on the floor. [`FloorReason::text`] is the line the screen and `audit verify` print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloorReason {
    /// Not absolute, not UTF-8, or has `.`, `..`, an empty component or a trailing `/`.
    NotClean,
    /// `/`.
    FilesystemRoot,
    /// A direct child of `/` (`/home`, `/var`, `/tmp`, `/srv`, `/mnt`, `/opt`, `/Users`, ...).
    TopLevel,
    /// Inside, or equal to, a system or application directory (`/usr`, `/etc`, `/boot`, `/bin`, `/sbin`, `/lib*`,
    /// `/dev`, `/proc`, `/sys`, `/run`, `/System`, `/Library`, `/opt`, `/nix`, `/snap`, `/efi`, `/lost+found`,
    /// `/Applications`, `/tmp/.X11-unix`).
    System,
    /// Inside `/var` but not below `/var/tmp`, `/var/cache` or `/var/folders` (macOS temp), or one of those three.
    VarState,
    /// Another account's home directory or anything below it (`/home/<user>`, `/Users/<user>`,
    /// `/var/home/<user>`, `/root`) that is not below the real `$HOME`.
    AccountHome,
    /// `$HOME` itself.
    Home,
    /// An ancestor of `$HOME`.
    HomeAncestor,
    /// Fewer than two components below `$HOME` (rule F of the delete engine).
    ShallowInHome,
    /// A key, credential or browser-profile directory, an ssh-agent socket directory in `/tmp`, or a directory
    /// that holds one.
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
                "only folders below /var/tmp, /var/cache and /var/folders can be deleted from /var"
            }
            FloorReason::AccountHome => "another account's home directory and what is in it are never deleted from here",
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
    let rel_home = home_rules(path, home);
    if let HomeRel::Refused(r @ (FloorReason::Home | FloorReason::HomeAncestor)) = rel_home {
        return Some(r);
    }
    if p.len() == 1 {
        return Some(FloorReason::TopLevel);
    }
    if SYSTEM_TOPS.iter().any(|n| eq_ci(p[0], n)) {
        return Some(FloorReason::System);
    }
    if let Some(r) = tmp_rule(&p) {
        return Some(r);
    }
    if eq_ci(p[0], "var") {
        let var_home = p.len() >= 3 && eq_ci(p[1], "home");
        // `folders` is macOS's per-user temp and cache area (`/var/folders/xx/<hash>/T`).
        let scratch = p.len() >= 3 && ["tmp", "cache", "folders"].iter().any(|n| eq_ci(p[1], n));
        if !(scratch || var_home) {
            return Some(FloorReason::VarState);
        }
    }
    match rel_home {
        HomeRel::Refused(r) => Some(r),
        HomeRel::Inside => None,
        HomeRel::Outside => other_account(&p),
    }
}

/// `/tmp/ssh-*` holds ssh-agent sockets; `/tmp/.X11-unix` holds the X server's.
fn tmp_rule(p: &[&OsStr]) -> Option<FloorReason> {
    if p.len() < 2 || !eq_ci(p[0], "tmp") {
        return None;
    }
    let name = p[1].as_bytes();
    if name.len() >= 4 && name[..4].eq_ignore_ascii_case(b"ssh-") {
        return Some(FloorReason::Secrets);
    }
    eq_ci(p[1], ".X11-unix").then_some(FloorReason::System)
}

/// Something in another account's tree: `/home/<user>/...`, `/Users/<user>/...`, `/var/home/<user>/...` or
/// `/root/...`, for a path that is not below the real `$HOME`.
fn other_account(p: &[&OsStr]) -> Option<FloorReason> {
    let in_account = match p {
        [top, _, ..] if eq_ci(top, "home") || eq_ci(top, "Users") || eq_ci(top, "root") => true,
        [var, home, _, ..] => eq_ci(var, "var") && eq_ci(home, "home"),
        _ => false,
    };
    in_account.then_some(FloorReason::AccountHome)
}

fn same_prefix(a: &[&OsStr], b: &[&OsStr]) -> bool {
    a.len() <= b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.as_bytes().eq_ignore_ascii_case(y.as_bytes()))
}

/// Where `path` stands against `$HOME`.
enum HomeRel {
    /// Not below `$HOME` (and not `$HOME` or an ancestor).
    Outside,
    /// Below `$HOME`, deep enough and not a secret.
    Inside,
    /// `$HOME`, an ancestor, shallow, or a secret.
    Refused(FloorReason),
}

fn home_rules(path: &Path, home: &Path) -> HomeRel {
    let (Some(h), Some(p)) = (parts(home), parts(path)) else {
        return HomeRel::Outside;
    };
    if p.len() == h.len() && same_prefix(&h, &p) {
        return HomeRel::Refused(FloorReason::Home);
    }
    if same_prefix(&p, &h) {
        return HomeRel::Refused(FloorReason::HomeAncestor);
    }
    if !same_prefix(&h, &p) {
        return HomeRel::Outside;
    }
    let rel = &p[h.len()..];
    if rel.len() < 2 {
        return HomeRel::Refused(FloorReason::ShallowInHome);
    }
    let secret = SECRET_DIRS.iter().any(|s| {
        let n = s.len().min(rel.len());
        rel[..n].iter().zip(&s[..n]).all(|(a, b)| eq_ci(a, b))
    });
    if secret {
        HomeRel::Refused(FloorReason::Secrets)
    } else {
        HomeRel::Inside
    }
}

/// [`explorer_floor`], and also `Secrets` for a path that is, is below or holds one of `extra` (the secret
/// directories a moved `XDG_*_HOME` or a symlinked secret directory put elsewhere: `RootSet::extra_secrets`).
pub fn explorer_floor_with(path: &Path, home: &Path, extra: &[PathBuf]) -> Option<FloorReason> {
    explorer_floor(path, home).or_else(|| {
        extra
            .iter()
            .any(|s| touches_secret(path, s))
            .then_some(FloorReason::Secrets)
    })
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
            "/mnt/disk/stuff",
            "/var/tmp/x",
            "/var/cache/x",
            "/var/tmp/x/y",
            "/private/var/folders/zz/abc/T/x",
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
        for p in ["/var", "/tmp", "/srv", "/mnt", "/opt", "/Users", "/root"] {
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
    fn explorer_floor_should_refuse_everything_below_another_accounts_home() {
        for p in [
            "/home/other/code",
            "/home/other/.ssh/x",
            "/Users/o/Documents/x",
            "/var/home/other/code/app",
            "/root/x",
            "/root/a/b",
        ] {
            assert_eq!(floor(p), Some(FloorReason::AccountHome), "{p}");
        }
        let root = |p: &str| explorer_floor(Path::new(p), Path::new("/root"));
        assert_eq!(root("/root/code/app"), None);
        assert_eq!(root("/root/.ssh/x"), Some(FloorReason::Secrets));
    }

    #[test]
    fn explorer_floor_should_refuse_application_trees_and_agent_sockets() {
        for p in [
            "/opt/thing",
            "/nix/store/x",
            "/snap/core/1",
            "/efi/x",
            "/lost+found/x",
            "/Applications/A.app/x",
            "/tmp/.X11-unix/X0",
        ] {
            assert_eq!(floor(p), Some(FloorReason::System), "{p}");
        }
        for p in ["/tmp/ssh-abc/agent.1", "/private/tmp/SSH-abc/agent.1"] {
            assert_eq!(floor(p), Some(FloorReason::Secrets), "{p}");
        }
        assert_eq!(floor("/tmp/ssh/x"), None);
        assert_eq!(floor("/tmp/sshfs/x"), None);
    }

    #[test]
    fn secret_dirs_should_list_every_default_and_follow_a_moved_xdg_home_below_home_only() {
        let home = Path::new("/home/u");
        let default = secret_dirs(home, None, None);
        assert_eq!(default.len(), SECRET_DIRS.len());
        assert!(default.contains(&PathBuf::from("/home/u/.aws")));
        let moved = secret_dirs(
            home,
            Some(Path::new("/home/u/cfg")),
            Some(Path::new("/home/u/data")),
        );
        assert!(moved.contains(&PathBuf::from("/home/u/cfg/gh")));
        assert!(moved.contains(&PathBuf::from("/home/u/data/keyrings")));
        let outside = secret_dirs(
            home,
            Some(Path::new("/srv/cfg")),
            Some(Path::new("/home/u")),
        );
        assert_eq!(outside, default);
        let unclean = secret_dirs(home, Some(Path::new("/home/u/../x")), None);
        assert_eq!(unclean, default);
    }

    #[test]
    fn touches_secret_should_match_the_directory_what_is_below_it_and_what_holds_it() {
        let s = Path::new("/mnt/vault/Keys");
        assert!(touches_secret(Path::new("/mnt/vault/keys/x/y"), s));
        assert!(touches_secret(Path::new("/mnt/vault"), s));
        assert!(touches_secret(Path::new("/mnt/vault/KEYS"), s));
        assert!(!touches_secret(Path::new("/mnt/vault/keys2/x"), s));
        assert!(!touches_secret(Path::new("/mnt/other"), s));
        assert!(!touches_secret(Path::new("relative"), s));
    }

    #[test]
    fn explorer_floor_with_should_add_the_extra_secret_directories() {
        let extra = [PathBuf::from("/mnt/vault/ssh")];
        let f = |p: &str| explorer_floor_with(Path::new(p), Path::new(HOME), &extra);
        assert_eq!(f("/mnt/vault/ssh/keys"), Some(FloorReason::Secrets));
        assert_eq!(f("/mnt/vault/other/x"), None);
        assert_eq!(f("/usr/x"), Some(FloorReason::System));
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

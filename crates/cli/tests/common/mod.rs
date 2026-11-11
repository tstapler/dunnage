//! End-to-end harness (Task 1.1.6b). A test binary includes it with `mod common;`.
//!
//! `fixture_cmd` builds the command for the binary under test from an empty environment plus an explicit
//! allowlist, so a developer's `HOME`, `XDG_*` or `DC_DEV_APPLY_REAL` can never reach it. The XDG directories
//! live beside the fixture home, not inside it: the walker cache and `last_plan` are written there, and the
//! hashed fixture home stays unchanged by a plan run.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Names `fixture_cmd` passes to the child, and nothing else.
pub const ENV_ALLOWLIST: [&str; 7] = [
    "PATH",
    "HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "DC_TEST_ROOT",
    "LANG",
];

/// The sentinel `confine_mode` (Story 2.1.6) requires under `DC_TEST_ROOT`.
pub const SENTINEL: &str = ".dc-test-root";

/// Escape hatch of the developer build; a test process that has it set is refused outright.
pub const DEV_APPLY_REAL: &str = "DC_DEV_APPLY_REAL";

/// `Err` names the variable that must not be set in the test process.
pub fn check_start_env(get: &dyn Fn(&str) -> Option<OsString>) -> Result<(), String> {
    match get(DEV_APPLY_REAL) {
        Some(_) => Err(format!(
            "{DEV_APPLY_REAL} is set in the test process environment; refusing to run any test"
        )),
        None => Ok(()),
    }
}

/// Aborts the whole test binary (SIGABRT, not one failed test) when `DC_DEV_APPLY_REAL` is set.
pub fn startup_check() {
    if let Err(msg) = check_start_env(&|k| std::env::var_os(k)) {
        eprintln!("{msg}");
        std::process::abort();
    }
}

/// A fixture root: `<root>/home`, `<root>/xdg/{state,cache,config}` and the sentinel. Removed on drop.
pub struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    pub fn new() -> Fixture {
        startup_check();
        let dir = tempfile::tempdir().expect("fixture root");
        for sub in ["home", "xdg/state", "xdg/cache", "xdg/config"] {
            std::fs::create_dir_all(dir.path().join(sub)).expect("fixture dir");
        }
        std::fs::write(dir.path().join(SENTINEL), "").expect("sentinel");
        Fixture { dir }
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    pub fn home(&self) -> PathBuf {
        self.root().join("home")
    }

    pub fn xdg(&self, which: &str) -> PathBuf {
        self.root().join("xdg").join(which)
    }

    /// The environment the child gets, as (name, value) pairs. `PATH` and `LANG` come from the test process.
    pub fn env(&self) -> Vec<(&'static str, OsString)> {
        // A `docker` whose daemon is down comes first, so no fixture run reaches the machine's real daemon
        // when a build registers the Docker kinds (gate-d). A test that sets its own `PATH` adds its own shim.
        // It lives outside the fixture root so the tree manifests of the safety tests do not see it.
        static SHIM_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        let shim_dir = SHIM_DIR
            .get_or_init(|| {
                let dir =
                    std::env::temp_dir().join(format!("dc-docker-down-{}", std::process::id()));
                docker_down_shim(&dir);
                dir
            })
            .clone();
        let path = std::env::join_paths(std::iter::once(shim_dir).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))
        .unwrap();
        vec![
            ("PATH", path),
            ("HOME", self.home().into()),
            ("XDG_STATE_HOME", self.xdg("state").into()),
            ("XDG_CACHE_HOME", self.xdg("cache").into()),
            ("XDG_CONFIG_HOME", self.xdg("config").into()),
            ("DC_TEST_ROOT", self.root().into()),
            (
                "LANG",
                std::env::var_os("LANG").unwrap_or_else(|| "C".into()),
            ),
        ]
    }

    /// Any program under the fixture environment (the harness tests use `env`).
    pub fn command_for(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        startup_check();
        let mut c = Command::new(program);
        c.env_clear();
        c.envs(self.env());
        c
    }

    /// The binary under test under the fixture environment.
    pub fn fixture_cmd(&self) -> Command {
        self.command_for(bin_path())
    }

    /// Runs plan mode with `--json`, reads its `plan_id`, then runs the same arguments with `--apply
    /// --plan-id <id>`: no test hard-codes an id.
    pub fn plan_then_apply(&self, args: &[&str]) -> Output {
        let plan = self
            .fixture_cmd()
            .args(args)
            .arg("--json")
            .output()
            .expect("plan run");
        let text = String::from_utf8_lossy(&plan.stdout).into_owned();
        let id = extract_plan_id(&text)
            .unwrap_or_else(|| panic!("no plan_id in the plan-mode output: {text}"));
        self.fixture_cmd()
            .args(args)
            .args(["--apply", "--plan-id", &id])
            .output()
            .expect("apply run")
    }
}

impl Default for Fixture {
    fn default() -> Fixture {
        Fixture::new()
    }
}

/// The `plan_id` of a `--json` document, else the id after `--plan-id` in a `next[]` command.
pub fn extract_plan_id(json: &str) -> Option<String> {
    let is_id = |s: &str| s.len() == 8 && s.bytes().all(|b| b.is_ascii_hexdigit());
    // Every `"plan_id"` key: a `next[]` entry can carry `null` before the top-level field does.
    for (at, _) in json.match_indices("\"plan_id\"") {
        let rest = json[at + "\"plan_id\"".len()..].trim_start();
        let Some(rest) = rest.strip_prefix(':').map(str::trim_start) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find('"') else { continue };
        if is_id(&rest[..end]) {
            return Some(rest[..end].to_string());
        }
    }
    let at = json.find("--plan-id ")?;
    let id: String = json[at + "--plan-id ".len()..]
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect();
    is_id(&id).then_some(id)
}

/// A manifest of every path below `dir` with its kind, size and a content hash (FNV-1a), for "unchanged"
/// assertions. Not cryptographic: it detects change, not tampering.
pub fn tree_manifest(dir: &Path) -> BTreeMap<PathBuf, String> {
    fn fnv(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
        })
    }
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap_or(&p).to_path_buf();
            let Ok(meta) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if meta.is_dir() {
                out.insert(rel, "dir".into());
                walk(base, &p, out);
            } else if meta.is_file() {
                let body = std::fs::read(&p).unwrap_or_default();
                out.insert(rel, format!("file {} {:016x}", meta.len(), fnv(&body)));
            } else {
                out.insert(rel, "other".into());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// `strace` is installed; the canary is a named skip without it unless `DC_REQUIRE_STRACE=1`.
pub fn strace_available() -> bool {
    Command::new("strace")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// `strace -f -e trace=file -o <log>` around `program`, under the fixture environment.
pub fn strace_cmd(fx: &Fixture, log: &Path, program: impl AsRef<std::ffi::OsStr>) -> Command {
    // An absolute path, so a test may give the traced program a different `PATH` than strace itself needs.
    let strace = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|d| d.join("strace"))
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("strace"));
    let mut c = fx.command_for(strace);
    c.args(["-f", "-e", "trace=file", "-o"]).arg(log);
    c.arg(program);
    c
}

/// The paths a `strace -f` log shows the traced program itself touching: the first quoted absolute path of
/// every file syscall, from the root process and its threads. A process that calls `execve` of another program
/// is a subprocess (a tool the binary runs through `CommandRunner`, which has its own rules) and is left out.
pub fn strace_own_paths(log: &str) -> Vec<String> {
    let parsed: Vec<(&str, &str)> = log
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(pid, rest)| (pid, rest.trim_start()))
        .collect();
    let Some(&(root_pid, _)) = parsed.first() else {
        return Vec::new();
    };
    let subprocess: std::collections::BTreeSet<&str> = parsed
        .iter()
        .filter(|(pid, rest)| *pid != root_pid && rest.starts_with("execve("))
        .map(|(pid, _)| *pid)
        .collect();
    let mut out = Vec::new();
    for (pid, rest) in parsed {
        if subprocess.contains(pid) {
            continue;
        }
        let Some((call, args)) = rest.split_once('(') else {
            continue;
        };
        if call.starts_with('<') || call.starts_with('+') || call.starts_with('-') {
            continue;
        }
        // openat(AT_FDCWD, "/p", ...), stat("/p", ...), readlink("/p", ...): the first quoted string
        let Some(start) = args.find('"') else {
            continue;
        };
        let tail = &args[start + 1..];
        let Some(end) = tail.find('"') else { continue };
        if tail[..end].starts_with('/') {
            out.push(tail[..end].to_string());
        }
    }
    out
}

/// Paths of `paths` that are neither under one of `allowed` prefixes nor an exact entry of `exact`, plus
/// `/etc/passwd` and `/etc/nsswitch.conf` always (a `getpwuid` regression reads them).
pub fn canary_violations(paths: &[String], allowed: &[PathBuf], exact: &[&str]) -> Vec<String> {
    let mut bad: Vec<String> = paths
        .iter()
        .filter(|p| {
            let path = Path::new(p.as_str());
            *p == "/etc/passwd"
                || *p == "/etc/nsswitch.conf"
                || !(allowed.iter().any(|a| path.starts_with(a)) || exact.contains(&p.as_str()))
        })
        .cloned()
        .collect();
    bad.sort();
    bad.dedup();
    bad
}

// ---------------------------------------------------------------- Story 2.3.3 helpers

use std::time::{Duration, SystemTime};

/// The binary under test as an absolute path. Cargo's `CARGO_BIN_EXE_*` already is; under Bazel it is relative to
/// the runfiles directory (the test's working directory), which a child started with `current_dir` would not find.
pub fn bin_path() -> PathBuf {
    std::env::current_dir()
        .expect("cwd")
        .join(env!("CARGO_BIN_EXE_disk-cleanup"))
}

/// The repository root (`CARGO_MANIFEST_DIR` is `crates/cli`).
pub fn repo_root() -> PathBuf {
    // Bazel runs tests in a runfiles tree of symlinks, which the repo-level scripts cannot walk: the tests that run
    // them are tagged `local` and get the real checkout through DC_REPO_ROOT (scripts/bazel.sh sets it).
    std::env::var_os("DC_REPO_ROOT").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."),
        PathBuf::from,
    )
}

/// `<dir>/f` holding `bytes` bytes, the file and the directory aged `days` days (so `--min-age` sees the age).
pub fn aged_dir(dir: &Path, bytes: usize, days: u64) {
    std::fs::create_dir_all(dir).expect("aged dir");
    let file = dir.join("f");
    std::fs::write(&file, vec![5u8; bytes]).expect("aged file");
    let then = SystemTime::now() - Duration::from_secs(days * 86_400);
    for p in [&file, &dir.to_path_buf()] {
        std::fs::File::open(p)
            .and_then(|f| f.set_modified(then))
            .expect("set mtime");
    }
}

/// The command lines of the first `Next:` block of a dry run: `^  (disk-cleanup|sudo ...) `; the explanation
/// under each line is indented by six spaces and never matches.
pub fn next_commands(stdout: &str) -> Vec<String> {
    let mut in_next = false;
    let mut out = Vec::new();
    for line in stdout.lines() {
        if line == "Next:" {
            in_next = true;
            continue;
        }
        if !in_next {
            continue;
        }
        if line.is_empty() {
            break;
        }
        let is_command = line.starts_with("  disk-cleanup ") || line.starts_with("  sudo ");
        // The balance hint appears only when the fixture HOME is on btrfs (TMPDIR on a btrfs volume), so it
        // is not part of what these flows count (Story 4.3.2).
        let host_btrfs_hint =
            line.starts_with("  sudo ") && line.ends_with(" --apply --kinds btrfs");
        if is_command && !line.starts_with("   ") && !host_btrfs_hint {
            out.push(line.trim().to_string());
        }
    }
    out
}

/// Runs a printed `Next:` command line under the fixture environment with stdin from `/dev/null`: the binary
/// name is swapped for the binary under test, nothing else changes. A `sudo` line is a test failure.
pub fn run_line(fx: &Fixture, line: &str) -> Output {
    assert!(
        !line.contains('\'') && !line.contains('"') && !line.starts_with("sudo "),
        "a fixture run cannot take this line: {line}"
    );
    let mut words = line.split_whitespace();
    assert_eq!(words.next(), Some("disk-cleanup"), "{line}");
    fx.fixture_cmd()
        .args(words)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the Next line")
}

pub fn text_of(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `unshare -rm` works here (rootless user and mount namespace). The rehearsal tests are a named skip without
/// it unless `DC_REQUIRE_UNSHARE=1`.
pub fn unshare_works() -> bool {
    let ok = Command::new("unshare")
        .args(["-rm", "true"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        assert!(
            std::env::var_os("DC_REQUIRE_UNSHARE").is_none(),
            "DC_REQUIRE_UNSHARE is set and `unshare -rm` does not work"
        );
        eprintln!(
            "SKIP: unshare -rm does not work here (DC_REQUIRE_UNSHARE=1 makes this a failure)"
        );
    }
    ok
}

/// names, sizes, modes and mtimes of everything below `dir` (what "unchanged" means for a dry run).
pub fn tree_hash(dir: &Path) -> BTreeMap<PathBuf, String> {
    use std::os::unix::fs::MetadataExt;
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(m) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            let rel = p.strip_prefix(base).unwrap_or(&p).to_path_buf();
            out.insert(
                rel,
                format!(
                    "size={} mode={:o} mtime={}.{:09}",
                    m.len(),
                    m.mode(),
                    m.mtime(),
                    m.mtime_nsec()
                ),
            );
            if m.is_dir() {
                walk(base, &p, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// A `docker` in `bin` whose daemon is down. A test PATH that ends in `/usr/bin` would otherwise reach the
/// machine's real daemon as soon as a build registers the Docker kinds (gate-d).
#[allow(dead_code)]
pub fn docker_down_shim(bin: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(bin).unwrap();
    let shim = bin.join("docker");
    std::fs::write(
        &shim,
        "#!/bin/sh\necho 'Cannot connect to the Docker daemon (test shim)' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
}

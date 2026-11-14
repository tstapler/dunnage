//! Cargo awareness for the `build-dirs` target: what a real cargo `target/` looks like, which of its profile
//! directories have been idle, and the fresh re-check of one part (`incremental/`, `.fingerprint/`, `deps/`,
//! `build/`, `examples/`) right before it is removed.
//!
//! DECISION (staleness): cargo touches `.fingerprint/*/invoked.timestamp` on every build, even one that finds
//! everything fresh and rewrites nothing in `deps/`. So `deps/`, `.fingerprint/`, `build/` and `examples/` are
//! judged by the activity of the whole profile (any file below it, or a direct entry of it, newer than the age
//! floor keeps all of them), and only `incremental/` by its own contents. `atime` is not used: `relatime` and
//! `noatime` mounts make it say nothing about use.
//!
//! Everything here reads with `lstat` and `DirOps` (`openat` with `O_NOFOLLOW`): a symlinked target, profile
//! or part is skipped, never followed.

use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use dunnage_core::targets::build_dir::effective_min_age;
use dunnage_core::targets::cargo::{
    decide_cargo_part, is_cargo_target, profile_selected, tag_has_signature, CargoPartFacts,
};
use dunnage_core::{
    AllocatedBytes, CargoPart, DeleteError, FileKind, Identity, Kind, RootDev, SkipReason,
};

use crate::dirops::{open_verified_root, DirOps};
use crate::discover_build::symlink_between;
use crate::mtime_probe::{to_system_time, unix_secs};
use crate::targets::build_dir::{gather, skip, BuildRecheck, Gathered, RecheckEnv};
use crate::treestat::{measure, MeasureOpts, SkipWhy, TreeStats};

/// Cargo's tag is a few dozen bytes; reading more would only read a file someone else wrote.
const TAG_READ_MAX: u64 = 128;

fn lstat(p: &Path) -> Option<fs::Metadata> {
    fs::symlink_metadata(p).ok()
}

fn is_plain_dir(p: &Path) -> bool {
    lstat(p).is_some_and(|m| m.file_type().is_dir())
}

fn is_plain_file(p: &Path) -> bool {
    lstat(p).is_some_and(|m| m.file_type().is_file())
}

/// `target` is a directory (not a link), sits next to a regular `Cargo.toml`, and holds cargo's
/// `CACHEDIR.TAG` (with cargo's signature) or its `.rustc_info.json`.
pub fn is_real_cargo_target(target: &Path) -> bool {
    if !is_plain_dir(target) || target.file_name() != Some(std::ffi::OsStr::new("target")) {
        return false;
    }
    let Some(project) = target.parent() else {
        return false;
    };
    if !is_plain_file(&project.join("Cargo.toml")) {
        return false;
    }
    let tag = target.join("CACHEDIR.TAG");
    let tag_ok = is_plain_file(&tag)
        && fs::File::open(&tag).is_ok_and(|f| {
            let mut buf = Vec::new();
            f.take(TAG_READ_MAX).read_to_end(&mut buf).is_ok() && tag_has_signature(&buf)
        });
    is_cargo_target(tag_ok, is_plain_file(&target.join(".rustc_info.json")))
}

/// A profile directory holds cargo's `.fingerprint/` or `deps/`.
fn is_profile_dir(p: &Path) -> bool {
    is_plain_dir(p) && (is_plain_dir(&p.join(".fingerprint")) || is_plain_dir(&p.join("deps")))
}

/// A directory below `target/` that cargo made for `--target <triple>`: it holds profiles, not parts.
fn looks_like_triple(name: &str) -> bool {
    name.contains('-') && !name.starts_with('.')
}

/// The profile directories of `target`, `debug`, `release`, a custom profile, and `<triple>/<profile>`, as
/// names relative to `target`. Symlinks are not listed.
pub fn profiles_of(target: &Path) -> Vec<(String, PathBuf)> {
    let mut names: Vec<String> = fs::read_dir(target)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        let dir = target.join(&name);
        if is_profile_dir(&dir) {
            out.push((name, dir));
        } else if looks_like_triple(&name) {
            let mut kids: Vec<String> = fs::read_dir(&dir)
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                        .filter_map(|e| e.file_name().into_string().ok())
                        .collect()
                })
                .unwrap_or_default();
            kids.sort();
            for kid in kids {
                let p = dir.join(&kid);
                if is_profile_dir(&p) {
                    out.push((format!("{name}/{kid}"), p));
                }
            }
        }
    }
    out
}

/// One part measured with the early-exit walk.
#[derive(Debug, Clone)]
pub struct PartMeasure {
    pub part: CargoPart,
    pub path: PathBuf,
    pub ident: Identity,
    pub allocated: AllocatedBytes,
    /// Newest mtime inside (or the directory's own when it is empty), Unix seconds.
    pub newest: i64,
    /// Something inside is newer than the floor.
    pub young: bool,
    /// The whole tree was read.
    pub complete: bool,
}

/// What a profile directory showed against the floor.
#[derive(Debug, Clone, Default)]
pub struct ProfileProbe {
    /// A direct entry of the profile, or any measured part, is newer than the floor.
    pub young: bool,
    /// The newest mtime seen anywhere in the profile (the first young one when `young`).
    pub activity: Option<i64>,
    /// The parts that were measured. Once the profile is young only `incremental/` still is, because it is
    /// the one part judged by itself.
    pub parts: Vec<PartMeasure>,
    /// A part could not be read completely and nothing young was seen: "stale" is not proven.
    pub unknown: bool,
}

impl ProfileProbe {
    fn note(&mut self, newest: i64) {
        self.activity = Some(self.activity.map_or(newest, |a| a.max(newest)));
    }

    /// The profile's activity as a staleness input: `None` when it could not be established.
    pub fn profile_activity(&self) -> Option<i64> {
        if self.unknown && !self.young {
            None
        } else {
            self.activity
        }
    }
}

fn mtime_secs(m: &fs::Metadata) -> i64 {
    m.mtime()
}

fn skipped_unreadable(stats: &TreeStats) -> bool {
    stats
        .skipped
        .iter()
        .any(|s| matches!(s.why, SkipWhy::Unreadable(_) | SkipWhy::TooDeep))
}

fn measure_part<D: DirOps>(
    ops: &D,
    part: CargoPart,
    path: &Path,
    root_dev: RootDev,
    floor: SystemTime,
) -> Result<Option<PartMeasure>, DeleteError> {
    let Some(m) = lstat(path) else {
        return Ok(None);
    };
    if !m.file_type().is_dir() {
        // a symlink or a file in a part's place: not a part, never followed
        return Ok(None);
    }
    let ident = Identity::of(&m);
    if !ident.on_dev(root_dev) {
        return Ok(None);
    }
    let root = open_verified_root(ops, path, ident)?;
    let stats = measure(
        ops,
        root.dir(),
        root_dev,
        MeasureOpts {
            stop_if_newer_than: Some(floor),
        },
    );
    let newest = stats.newest_mtime.map_or_else(|| mtime_secs(&m), unix_secs);
    let young = stats.newest_mtime.is_some_and(|n| n > floor) || stats.stopped_early;
    Ok(Some(PartMeasure {
        part,
        path: path.to_path_buf(),
        ident,
        allocated: stats.allocated,
        newest,
        young,
        complete: !stats.stopped_early && !skipped_unreadable(&stats),
    }))
}

/// Probes one profile directory against `floor`. Cheapest evidence first: the profile's own mtime and its
/// direct entries (final binaries, `.cargo-lock`), then `.fingerprint/` (touched by every build), `deps/`,
/// `build/`, `examples/`, and `incremental/` last.
pub fn probe_profile<D: DirOps>(
    ops: &D,
    profile: &Path,
    root_dev: RootDev,
    floor: SystemTime,
) -> Result<ProfileProbe, DeleteError> {
    if !is_plain_dir(profile) {
        return Err(DeleteError::Symlink);
    }
    let mut probe = ProfileProbe::default();
    let floor_secs = unix_secs(floor);
    let seen_young = |probe: &mut ProfileProbe, newest: i64| {
        probe.note(newest);
        if newest > floor_secs {
            probe.young = true;
        }
    };
    // The profile's own mtime says nothing: removing a part moves it, and the parts of one profile are
    // removed one after the other.
    match fs::read_dir(profile) {
        Ok(rd) => {
            for e in rd.flatten() {
                let is_part = e
                    .file_name()
                    .to_str()
                    .and_then(CargoPart::from_dir_name)
                    .is_some();
                if is_part && e.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                if let Ok(m) = e.metadata() {
                    seen_young(&mut probe, mtime_secs(&m));
                }
            }
        }
        Err(_) => probe.unknown = true,
    }
    let order = [
        CargoPart::Fingerprint,
        CargoPart::Deps,
        CargoPart::Build,
        CargoPart::Examples,
        CargoPart::Incremental,
    ];
    for part in order {
        if probe.young && part != CargoPart::Incremental {
            continue;
        }
        match measure_part(ops, part, &profile.join(part.dir_name()), root_dev, floor) {
            Ok(Some(pm)) => {
                seen_young(&mut probe, pm.newest);
                if pm.young {
                    probe.young = true;
                }
                if !pm.complete && !pm.young {
                    probe.unknown = true;
                }
                probe.parts.push(pm);
            }
            Ok(None) | Err(DeleteError::Io(libc::ENOENT)) => {}
            // another part changed under us: "stale" is not proven
            Err(_) => probe.unknown = true,
        }
    }
    Ok(probe)
}

/// One stale part found by [`find_stale_parts`].
#[derive(Debug, Clone)]
pub struct FoundPart {
    pub part: CargoPart,
    pub path: PathBuf,
    pub profile: String,
    pub ident: Identity,
    pub bytes: AllocatedBytes,
    /// The activity that made it stale (Unix seconds): the part's own newest mtime for `incremental/`, the
    /// profile's for the rest.
    pub activity: i64,
}

/// The parts of `profile` that `probe` proves stale.
pub fn stale_parts(profile: &str, probe: &ProfileProbe) -> Vec<FoundPart> {
    let profile_activity = (!probe.young && !probe.unknown)
        .then_some(probe.activity)
        .flatten();
    probe
        .parts
        .iter()
        .filter_map(|pm| {
            let activity = if pm.part.follows_profile() {
                profile_activity?
            } else if !pm.young && pm.complete {
                pm.newest
            } else {
                return None;
            };
            Some(FoundPart {
                part: pm.part,
                path: pm.path.clone(),
                profile: profile.to_string(),
                ident: pm.ident,
                bytes: pm.allocated,
                activity,
            })
        })
        .collect()
}

/// The stale parts of every selected profile of `target`. Empty for anything that is not a real cargo
/// target. `floor_secs` is the age floor (at least 60 minutes), `now` Unix seconds.
pub fn find_stale_parts<D: DirOps>(
    ops: &D,
    target: &Path,
    root_dev: RootDev,
    now: i64,
    floor_secs: i64,
    profiles: &[String],
) -> Vec<FoundPart> {
    if !is_real_cargo_target(target) {
        return Vec::new();
    }
    let floor = to_system_time(now.saturating_sub(floor_secs), 0);
    let mut out = Vec::new();
    for (name, dir) in profiles_of(target) {
        if !profile_selected(&name, profiles) {
            continue;
        }
        if let Ok(probe) = probe_profile(ops, &dir, root_dev, floor) {
            out.extend(stale_parts(&name, &probe));
        }
    }
    out
}

/// `<project>/target/<profile>/<part>` or `<project>/target/<triple>/<profile>/<part>` taken apart.
pub struct PartPath<'a> {
    pub part: CargoPart,
    pub profile_dir: &'a Path,
    pub target: &'a Path,
    /// `debug` or `<triple>/debug`.
    pub profile: String,
}

impl<'a> PartPath<'a> {
    pub fn parse(path: &'a Path) -> Option<PartPath<'a>> {
        let part = CargoPart::from_dir_name(path.file_name()?.to_str()?)?;
        let profile_dir = path.parent()?;
        let is_target = |p: &Path| p.file_name() == Some(std::ffi::OsStr::new("target"));
        let above = profile_dir.parent()?;
        let (target, profile) = if is_target(above) {
            (above, profile_dir.file_name()?.to_str()?.to_string())
        } else {
            let t = above.parent()?;
            if !is_target(t) {
                return None;
            }
            (
                t,
                format!(
                    "{}/{}",
                    above.file_name()?.to_str()?,
                    profile_dir.file_name()?.to_str()?
                ),
            )
        };
        Some(PartPath {
            part,
            profile_dir,
            target,
            profile,
        })
    }
}

/// Re-validates one part against fresh state: identity, cargo's markers, and the profile activity (judged
/// by the part itself for `incremental/`). `min_age` is the `--min-age` flag; the 60 minute floor is applied
/// by the pure decision.
pub fn recheck_cargo_part<D: DirOps>(
    env: &RecheckEnv<'_, D>,
    path: &Path,
    planned: Identity,
    now: i64,
    min_age: Duration,
) -> BuildRecheck {
    let Some(pp) = PartPath::parse(path) else {
        return skip(SkipReason::OutsideRoot);
    };
    let Gathered {
        st,
        root_dev,
        path_facts,
    } = match gather(env, Kind::CargoArtifact, path) {
        Ok(g) => g,
        Err(r) => return r,
    };
    // The profile's `.fingerprint/` or `deps/` may already be gone: the parts of one profile are removed one
    // after the other. That the part itself is a plain directory is checked by `cheap_ok` below.
    let structure_ok = is_real_cargo_target(pp.target);
    let cheap_ok = st.kind == FileKind::Dir
        && !path_facts.is_symlink
        && st.ident == planned
        && structure_ok
        && st.ident.on_dev(root_dev)
        && !path_facts.is_mount_point
        && !env
            .roots
            .root_of(path)
            .is_some_and(|r| symlink_between(r, path));
    let activity = if cheap_ok {
        let (eff, _) = effective_min_age(min_age);
        let floor_secs = i64::try_from(eff.as_secs()).unwrap_or(i64::MAX);
        let floor = to_system_time(now.saturating_sub(floor_secs), 0);
        match probe_profile(env.ops, pp.profile_dir, root_dev, floor) {
            Ok(probe) => activity_of(&pp, &probe),
            Err(DeleteError::IdentityMismatch) => return skip(SkipReason::IdentityChanged),
            Err(DeleteError::Io(libc::ENOENT)) => return skip(SkipReason::Gone),
            Err(DeleteError::Symlink) => return skip(SkipReason::Symlink),
            Err(_) => None,
        }
    } else {
        None
    };
    let facts = CargoPartFacts {
        path: path_facts,
        planned,
        structure_ok,
        activity_mtime: activity,
    };
    let guard = decide_cargo_part(&facts, env.roots, now, min_age);
    BuildRecheck {
        guard,
        newest_mtime: activity,
    }
}

fn activity_of(pp: &PartPath<'_>, probe: &ProfileProbe) -> Option<i64> {
    if pp.part.follows_profile() {
        return probe.profile_activity();
    }
    // a part that could not be measured has no activity: fail closed
    probe
        .parts
        .iter()
        .find(|m| m.part == pp.part)
        .filter(|m| m.young || m.complete)
        .map(|m| m.newest)
}

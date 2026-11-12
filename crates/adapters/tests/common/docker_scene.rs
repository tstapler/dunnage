#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unnecessary_wraps,
    clippy::unused_self
)]
//! A scripted Docker daemon for the target tests: images, containers, volumes and build cache described as
//! data, answered through a `FakeCommandRunner` in the exact shapes of the 29.8.2 fixtures. Nothing here can
//! reach a real daemon.

use std::path::PathBuf;
use std::time::Duration;

use dunnage_adapters::targets::docker_common::DOCKER_IDENT;
use dunnage_core::audit_types::RunRoots;
use dunnage_core::executor::{Executor, Mode, RunMeta, RunOptions, RunReport};
use dunnage_core::ports::{CmdOutput, Env};
use dunnage_core::selector::{KindSelector, Selection};
use dunnage_core::target::Target;
use dunnage_core::testing::{
    FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs, FakePlanStore, FakeProbe, FakeRunGuard,
    FakeSpaceMeter,
};
use dunnage_core::{ApplyToken, BuildInfo, Dirs, SelectorId, Status};

pub const NOW: i64 = 1_800_000_000;
pub const DAY: i64 = 86_400;
pub const GIB: u64 = 1 << 30;

pub const CTX: &[&str] = &["docker", "context", "show"];
pub const INFO: &[&str] = &["docker", "info"];
pub const VERSION: &[&str] = &["docker", "version", "--format", "{{.Server.Version}}"];
pub const DF: &[&str] = &["docker", "system", "df", "-v", "--format", "json"];
pub const PS: &[&str] = &["docker", "ps", "-a", "--format", "{{json .}}"];
pub const IMAGES: &[&str] = &["docker", "images", "--format", "{{json .}}"];
pub const VOLS: &[&str] = &["docker", "volume", "ls", "--format", "{{json .}}"];

pub fn ok(s: &str) -> Result<CmdOutput, dunnage_core::DeleteError> {
    Ok(CmdOutput {
        stdout: s.as_bytes().to_vec(),
        stderr: Vec::new(),
    })
}

pub fn failed(message: &str) -> Result<CmdOutput, dunnage_core::DeleteError> {
    Err(dunnage_core::DeleteError::CommandFailed {
        status: 1,
        message: message.to_string(),
    })
}

fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `docker images` style: `2026-11-01 18:36:31 +0000 UTC`.
pub fn stamp(epoch: i64) -> String {
    let (y, m, d) = civil(epoch.div_euclid(DAY));
    let s = epoch.rem_euclid(DAY);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} +0000 UTC",
        s / 3600,
        s % 3600 / 60,
        s % 60
    )
}

/// `volume inspect` style RFC 3339.
pub fn rfc3339(epoch: i64) -> String {
    let (y, m, d) = civil(epoch.div_euclid(DAY));
    let s = epoch.rem_euclid(DAY);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        s % 3600 / 60,
        s % 60
    )
}

#[derive(Clone)]
pub struct Img {
    pub id: String,
    pub repo: Option<String>,
    pub tag: Option<String>,
    pub digest: Option<String>,
    pub size: String,
    pub age_days: i64,
    pub containers: u32,
}

impl Img {
    pub fn untagged(id: &str, size: &str) -> Img {
        Img {
            id: id.into(),
            repo: None,
            tag: None,
            digest: None,
            size: size.into(),
            age_days: 5,
            containers: 0,
        }
    }

    pub fn tagged(id: &str, repo: &str, tag: &str, age_days: i64) -> Img {
        Img {
            id: id.into(),
            repo: Some(repo.into()),
            tag: Some(tag.into()),
            digest: Some("sha256:0123abcd".into()),
            size: "100MB".into(),
            age_days,
            containers: 0,
        }
    }

    fn line(&self) -> String {
        serde_json::json!({
            "Containers": self.containers.to_string(),
            "CreatedAt": stamp(NOW - self.age_days * DAY),
            "CreatedSince": "x",
            "Digest": self.digest.clone().unwrap_or_else(|| "<none>".into()),
            "ID": self.id,
            "Repository": self.repo.clone().unwrap_or_else(|| "<none>".into()),
            "SharedSize": "N/A",
            "Size": self.size,
            "Tag": self.tag.clone().unwrap_or_else(|| "<none>".into()),
            "UniqueSize": "N/A",
        })
        .to_string()
    }
}

#[derive(Clone)]
pub struct Vol {
    pub name: String,
    pub anonymous: bool,
    pub labels: Vec<(String, String)>,
    pub links: u32,
    pub size: String,
    pub age_days: i64,
    pub files: Option<Vec<String>>,
}

impl Vol {
    pub fn anon(age_days: i64) -> Vol {
        Vol {
            name: "a".repeat(63) + "1",
            anonymous: true,
            labels: Vec::new(),
            links: 0,
            size: "1.049MB".into(),
            age_days,
            files: Some(vec!["data.txt".into()]),
        }
    }

    pub fn named(name: &str, age_days: i64) -> Vol {
        Vol {
            name: name.into(),
            anonymous: false,
            labels: Vec::new(),
            links: 0,
            size: "2.097MB".into(),
            age_days,
            files: Some(vec!["data.txt".into()]),
        }
    }

    pub fn mountpoint(&self) -> String {
        format!("/var/lib/docker/volumes/{}/_data", self.name)
    }

    fn label_string(&self) -> String {
        let mut l: Vec<String> = self
            .labels
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        if self.anonymous {
            l.push("com.docker.volume.anonymous=".into());
        }
        l.join(",")
    }
}

#[derive(Clone)]
pub struct Ctr {
    pub id: String,
    pub name: String,
    pub mounts: String,
    pub image: String,
    pub state: String,
}

#[derive(Clone, Default)]
pub struct Scene {
    pub images: Vec<Img>,
    pub containers: Vec<Ctr>,
    pub volumes: Vec<Vol>,
    /// (id, size, in use)
    pub cache: Vec<(String, String, bool)>,
}

impl Scene {
    pub fn df(&self) -> String {
        serde_json::json!({
            "Images": self.images.iter().map(|i| serde_json::json!({"ID": i.id})).collect::<Vec<_>>(),
            "Containers": self.containers.iter().map(|c| serde_json::json!({"ID": c.id})).collect::<Vec<_>>(),
            "Volumes": self.volumes.iter().map(|v| serde_json::json!({
                "Name": v.name, "Labels": v.label_string(), "Links": v.links.to_string(),
                "Mountpoint": v.mountpoint(), "Size": v.size,
            })).collect::<Vec<_>>(),
            "BuildCache": self.cache.iter().map(|(id, size, in_use)| serde_json::json!({
                "ID": id, "InUse": in_use.to_string(), "Size": size,
            })).collect::<Vec<_>>(),
        })
        .to_string()
    }

    fn ps_lines(&self, only_mounting: Option<&str>) -> String {
        self.containers
            .iter()
            .filter(|c| only_mounting.is_none_or(|n| c.mounts.contains(n)))
            .map(|c| {
                serde_json::json!({"ID": c.id, "Names": c.name, "Mounts": c.mounts,
                    "State": c.state, "Image": c.image})
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn inspect(&self, v: &Vol) -> String {
        let labels = if v.anonymous || !v.labels.is_empty() {
            let mut m = serde_json::Map::new();
            for (k, val) in &v.labels {
                m.insert(k.clone(), val.clone().into());
            }
            if v.anonymous {
                m.insert("com.docker.volume.anonymous".into(), "".into());
            }
            serde_json::Value::Object(m)
        } else {
            serde_json::Value::Null
        };
        format!(
            "[{}]",
            serde_json::json!({"CreatedAt": rfc3339(NOW - v.age_days * DAY), "Driver": "local",
                "Labels": labels, "Mountpoint": v.mountpoint(), "Name": v.name,
                "Options": null, "Scope": "local"})
        )
    }

    /// A healthy 29.8.2 daemon: every read-only command the targets issue is answered.
    pub fn runner(&self) -> FakeCommandRunner {
        self.runner_without(&[])
    }

    /// The healthy daemon without the replies for the argvs in `skip` (the fake appends replies), so a test
    /// registers its own.
    pub fn runner_without(&self, skip: &[&[&str]]) -> FakeCommandRunner {
        let r = FakeCommandRunner::new();
        let vols = self
            .volumes
            .iter()
            .map(|v| {
                serde_json::json!({"Availability": "N/A", "Driver": "local", "Group": "N/A",
                    "Labels": v.label_string(), "Links": "N/A", "Mountpoint": v.mountpoint(),
                    "Name": v.name, "Scope": "local", "Size": "N/A", "Status": "N/A"})
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let table: Vec<(&[&str], String)> = vec![
            (CTX, "default\n".into()),
            (INFO, "Server:\n".into()),
            (VERSION, "29.8.2\n".into()),
            (DF, self.df()),
            (PS, self.ps_lines(None)),
            (
                IMAGES,
                self.images
                    .iter()
                    .map(Img::line)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            (VOLS, vols),
        ];
        for (argv, out) in table {
            if !skip.contains(&argv) {
                r.on(argv, ok(&out));
            }
        }
        for v in &self.volumes {
            r.on(
                &["docker", "volume", "inspect", &v.name],
                ok(&self.inspect(v)),
            );
            let filter = format!("volume={}", v.name);
            r.on(
                &[
                    "docker",
                    "ps",
                    "-a",
                    "--filter",
                    &filter,
                    "--format",
                    "{{json .}}",
                ],
                ok(&self.ps_lines(Some(&v.name))),
            );
        }
        r
    }

    /// The fake filesystem holding each volume's mount point.
    pub fn fs(&self) -> FakeFs {
        let mut fs = FakeFs::new();
        for v in &self.volumes {
            let mp = PathBuf::from(v.mountpoint());
            match &v.files {
                Some(files) => {
                    fs.add_dir(&mp);
                    for f in files {
                        fs.add_file(mp.join(f), "x");
                    }
                }
                None => {
                    fs.fail(&mp, 13);
                }
            }
        }
        fs
    }
}

pub struct Rig {
    pub fs: FakeFs,
    pub runner: FakeCommandRunner,
    pub clock: FakeClock,
    pub probe: FakeProbe,
    pub space: FakeSpaceMeter,
    pub audit: FakeAuditSink,
    pub guard: FakeRunGuard,
    pub plans: FakePlanStore,
}

impl Rig {
    pub fn new(scene: &Scene) -> Rig {
        Rig::with(scene.runner(), scene.fs())
    }

    pub fn with(runner: FakeCommandRunner, fs: FakeFs) -> Rig {
        Rig {
            fs,
            runner,
            clock: FakeClock::at(NOW),
            probe: FakeProbe::new(),
            space: FakeSpaceMeter::new(
                vec![FakeSpaceMeter::plain_snapshot("/", 500, 1000)],
                FakeSpaceMeter::stable(),
            ),
            audit: FakeAuditSink::new(),
            guard: FakeRunGuard::new(),
            plans: FakePlanStore::new(),
        }
    }

    pub fn env(&self) -> Env<'_> {
        Env {
            fs: &self.fs,
            runner: &self.runner,
            clock: &self.clock,
            probe: &self.probe,
            space: &self.space,
            audit: &self.audit,
            guard: &self.guard,
            plans: &self.plans,
            dirs: Dirs {
                home: "/h".into(),
                state: "/h/.local/state".into(),
                cache: "/h/.cache".into(),
                config: "/h/.config".into(),
            },
        }
    }
}

pub fn options() -> RunOptions {
    RunOptions {
        jobs: 1,
        settle_timeout: Duration::from_secs(1),
        meter_path: None,
        meta: RunMeta {
            run_id: "run-1".into(),
            build: BuildInfo {
                bin_version: "0.1.0".into(),
                git_commit: "abc".into(),
                dirty: false,
                metascope_rev: "none".into(),
                patched: false,
                metascope_dirty: false,
            },
            stamp: None,
            break_glass: None,
            roots: RunRoots::default(),
            verdict_start: None,
        },
    }
}

pub fn select(ids: &[SelectorId]) -> Selection {
    Selection::new(KindSelector::of(ids), KindSelector::default())
}

/// Plan, save, then apply with the plan's id: the two-command flow in process.
pub fn plan_then_apply(targets: Vec<&dyn Target>, env: &Env, sel: &Selection) -> RunReport {
    let ex = Executor::new(env, targets, options());
    let planned = ex
        .run(sel, &Mode::Plan, None)
        .unwrap_or_else(|r| panic!("plan refused: {}", r.text));
    let now = env.clock.now_epoch();
    env.plans.save_merged(
        vec![planned.plan.saved_selection(sel, env.probe.euid(), now)],
        env.probe.euid(),
        now,
    );
    ex.run(
        sel,
        &Mode::Apply(ApplyToken::for_tests()),
        Some(planned.plan_id),
    )
    .unwrap_or_else(|r| panic!("apply refused: {}", r.text))
}

pub fn status_for(r: &RunReport, c: &dunnage_core::Candidate) -> Status {
    r.summary
        .outcomes
        .iter()
        .find(|o| o.id == c.id())
        .unwrap_or_else(|| panic!("no outcome for {:?}", c.locator()))
        .status
        .clone()
}

pub fn ident() -> dunnage_core::Identity {
    DOCKER_IDENT
}

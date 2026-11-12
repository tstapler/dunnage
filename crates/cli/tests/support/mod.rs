#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! Fixtures for the rendering tests: the plan of the S1 sample of `design/ux.md` and its apply run.

use std::collections::HashMap;
use std::path::PathBuf;

use dunnage_cli::terminal::Terminal;
use dunnage_cli::view::{FsBlock, Options, PlanInput, ScanFacts};
use dunnage_core::candidate::{
    BuildDirFacts, BuildDirFlavor, CacheFacts, Candidate, DockerFacts, KindFacts, Locator,
    SteamFacts, WorktreeFacts,
};
use dunnage_core::ids::{AllocatedBytes, AppId, DockerId, Identity};
use dunnage_core::kind::SelectorId;
use dunnage_core::outcome::SkipReason;
use dunnage_core::plan::Plan;
use dunnage_core::plan_state::SaveOutcome;
use dunnage_core::selector::{KindSelector, Selection};
use dunnage_core::snapshot_types::{AllocSnapshot, FsSnapshot, SpaceUse};
use dunnage_core::summary::DiskVerdict;
use dunnage_core::target::Guarded;

pub const GIB: u64 = 1 << 30;
pub const TIB: u64 = 1 << 40;
pub const NOW: i64 = 1_800_000_000;
pub const HOME: &str = "/home/tstapler";
pub const EXE: &str = "/home/tstapler/.cargo/bin/dunnage";
const DAY: i64 = 86_400;

pub fn gib(x: f64) -> u64 {
    (x * GIB as f64).round() as u64
}

fn ident(n: u64) -> Identity {
    Identity { dev: 1, ino: n }
}

pub fn cache(path: &str, gibs: f64) -> Candidate {
    Candidate::new(
        Locator::Path(path.into()),
        KindFacts::Cache(CacheFacts {
            label: "cache".into(),
            apt: false,
        }),
        AllocatedBytes(gib(gibs)),
        None,
        ident(1),
    )
}

pub fn cache_apt(path: &str, gibs: f64) -> Candidate {
    Candidate::new(
        Locator::Path(path.into()),
        KindFacts::Cache(CacheFacts {
            label: "apt".into(),
            apt: true,
        }),
        AllocatedBytes(gib(gibs)),
        None,
        ident(8),
    )
}

pub fn worktree(path: &str, gibs: f64, age_days: i64) -> Candidate {
    Candidate::new(
        Locator::Path(path.into()),
        KindFacts::Worktree(WorktreeFacts {
            main_repo: "/home/tstapler/code/app".into(),
            branch: Some("agent".into()),
        }),
        AllocatedBytes(gib(gibs)),
        Some(NOW - age_days * DAY),
        ident(2),
    )
}

pub fn build_dir(path: &str, gibs: f64, age_days: i64, flavor: BuildDirFlavor) -> Candidate {
    Candidate::new(
        Locator::Path(path.into()),
        KindFacts::BuildDir(BuildDirFacts {
            flavor,
            group: "/home/tstapler/code".into(),
            age_secs: (age_days * DAY) as u64,
            lockfile_hash: None,
        }),
        AllocatedBytes(gib(gibs)),
        Some(NOW - age_days * DAY),
        ident(3),
    )
}

pub fn docker_dangling(gibs: f64) -> Candidate {
    Candidate::new(
        Locator::Docker(DockerId("dangling-images".into())),
        KindFacts::Docker(DockerFacts::DanglingImage {
            image_id: "sha256:ab12".into(),
        }),
        AllocatedBytes(gib(gibs)),
        None,
        ident(4),
    )
}

pub fn docker_volumes(gibs: f64) -> Candidate {
    Candidate::new(
        Locator::Docker(DockerId("volume-pg".into())),
        KindFacts::Docker(DockerFacts::Volume {
            name: "pg".into(),
            anonymous: true,
        }),
        AllocatedBytes(gib(gibs)),
        Some(NOW - 40 * DAY),
        ident(5),
    )
}

pub fn steam(path: &str, gibs: f64, age_days: i64) -> Candidate {
    Candidate::new(
        Locator::Steam(AppId(440), path.into()),
        KindFacts::Steam(SteamFacts::Prefix { app_id: AppId(440) }),
        AllocatedBytes(gib(gibs)),
        Some(NOW - age_days * DAY),
        ident(6),
    )
}

pub fn default_selection() -> Selection {
    Selection::new(KindSelector::default_apply(), KindSelector::default())
}

pub fn selection_of(ids: &[SelectorId]) -> Selection {
    Selection::new(KindSelector::of(ids), KindSelector::default())
}

pub fn plan_of(sel: &Selection, candidates: Vec<Candidate>) -> Plan {
    Plan::build(sel, NOW, candidates, &|c| {
        if c.kind() == dunnage_core::kind::Kind::BtrfsBalance {
            return Guarded::Skip(SkipReason::NeedsRoot);
        }
        if c.locator()
            == &Locator::Path("/home/tstapler/code/app/.claude/worktrees/agent-c19".into())
        {
            Guarded::Skip(SkipReason::Dirty)
        } else {
            Guarded::Allow
        }
    })
}

/// The candidates of the S1 sample.
pub fn s1_candidates() -> Vec<Candidate> {
    vec![
        build_dir(
            "/home/tstapler/code/metascope/target",
            38.4,
            41,
            BuildDirFlavor::CargoTarget,
        ),
        build_dir(
            "/home/tstapler/old-app/node_modules",
            21.7,
            63,
            BuildDirFlavor::NodeModules,
        ),
        docker_dangling(18.2),
        steam(
            "/home/tstapler/.local/share/Steam/steamapps/compatdata/440",
            16.5,
            90,
        ),
        docker_volumes(11.0),
        cache("/home/tstapler/.cache/go-build", 9.8),
        cache("/home/tstapler/.npm/_cacache", 4.1),
        cache("/home/tstapler/.cache/pip", 2.3),
        worktree(
            "/home/tstapler/code/app/.claude/worktrees/agent-a41",
            1.9,
            19,
        ),
        worktree(
            "/home/tstapler/code/app/.claude/worktrees/agent-c19",
            1.2,
            22,
        ),
    ]
}

pub fn btrfs_balance() -> Candidate {
    Candidate::new(
        Locator::Path("/".into()),
        KindFacts::Btrfs(dunnage_core::candidate::BtrfsFacts::Balance { usage_pct: 50 }),
        AllocatedBytes(0),
        None,
        ident(7),
    )
}

pub fn fs_btrfs() -> FsBlock {
    FsBlock {
        snapshot: FsSnapshot {
            path: "/".into(),
            taken_at: NOW,
            total_bytes: (1.82 * TIB as f64) as u64,
            used_bytes: (1.62 * TIB as f64) as u64,
            avail_bytes: gib(70.2),
            fs_type: "btrfs".into(),
            btrfs: Some(AllocSnapshot {
                data: SpaceUse {
                    bytes_used: (1.69 * TIB as f64) as u64,
                    total_bytes: (1.74 * TIB as f64) as u64,
                },
                metadata: SpaceUse {
                    bytes_used: gib(14.1),
                    total_bytes: gib(16.0),
                },
                system: SpaceUse {
                    bytes_used: 1 << 20,
                    total_bytes: 8 << 20,
                },
                unallocated: gib(9.8),
            }),
        },
        verdict: DiskVerdict::Warn,
    }
}

pub struct Fixture {
    pub plan: Plan,
    pub selection: Selection,
}

impl Fixture {
    pub fn s1() -> Fixture {
        let selection = default_selection();
        let plan = plan_of(&selection, s1_candidates());
        Fixture { plan, selection }
    }

    /// The S1 sample plus a btrfs row that needs root: the sample's fourth `Next:` command.
    pub fn s1_root() -> Fixture {
        let mut c = s1_candidates();
        c.push(btrfs_balance());
        Fixture::of(default_selection(), c)
    }

    pub fn of(selection: Selection, candidates: Vec<Candidate>) -> Fixture {
        let plan = plan_of(&selection, candidates);
        Fixture { plan, selection }
    }

    pub fn input(&self) -> PlanInput<'_> {
        PlanInput {
            plan: &self.plan,
            selection: &self.selection,
            saved: SaveOutcome::Saved,
            scan: ScanFacts {
                roots: 3,
                secs: 6.2,
                cache_warm: true,
                backend: "walker".into(),
                dirs: 41_288,
                unreadable: 0,
            },
            fs: Some(fs_btrfs()),
            partial_scan: None,
            not_built: Vec::new(),
            notes: Vec::new(),
            wrapper_note: None,
            exe: EXE.into(),
            euid: 1000,
            state_dir: "/home/tstapler/.local/state/dunnage".into(),
            home: Some(HOME.into()),
            facts: HashMap::new(),
            opts: Options::default(),
            errors: Vec::new(),
            tty: false,
        }
    }
}

pub fn pipe(width: usize) -> Terminal {
    Terminal {
        tty: false,
        width,
        color: false,
    }
}

pub fn pty(width: usize) -> Terminal {
    Terminal {
        tty: true,
        width,
        color: false,
    }
}

pub fn path(s: &str) -> PathBuf {
    s.into()
}

// ------------------------------------------------------------------ apply fixtures

use std::ffi::OsString;

use dunnage_cli::apply::ApplyInput;
use dunnage_core::delete_types::Freed;
use dunnage_core::ids::CandidateId;
use dunnage_core::kind::Kind;
use dunnage_core::outcome::{Outcome, Status};
use dunnage_core::plan::PlanId;
use dunnage_core::ports::ExtentAttribution;
use dunnage_core::snapshot_types::Settled;
use dunnage_core::summary::{FinishInput, RunSummary, StageTimings, WalkerCounters};

pub fn outcome(
    kind: Kind,
    sel: SelectorId,
    path: Option<&str>,
    status: Status,
    gibs: f64,
) -> Outcome {
    let locator = path.map_or_else(
        || Locator::Docker(DockerId(format!("{}-{gibs}", sel.id()))),
        |p| Locator::Path(p.into()),
    );
    let done = status == Status::Removed;
    let bytes = gib(gibs);
    Outcome {
        ts: NOW,
        run_id: "r1".into(),
        kind,
        selector: sel,
        id: CandidateId::new(kind, &locator),
        path: path.map(Into::into),
        status,
        est_bytes: bytes,
        measured_bytes: done.then_some(bytes),
        freed: Freed {
            attributed_bytes: AllocatedBytes(if done { bytes } else { 0 }),
            ..Freed::default()
        },
        elapsed_ms: 1_100,
        trimmed_bytes: None,
    }
}

pub fn s3_outcomes() -> Vec<Outcome> {
    use SelectorId as S;
    vec![
        outcome(
            Kind::CargoTarget,
            S::BuildDirs,
            Some("/home/tstapler/code/metascope/target"),
            Status::Removed,
            38.4,
        ),
        outcome(
            Kind::NodeModules,
            S::BuildDirs,
            Some("/home/tstapler/code/old/node_modules"),
            Status::Removed,
            18.0,
        ),
        outcome(
            Kind::Cache,
            S::Caches,
            Some("/home/tstapler/.cache/go-build"),
            Status::Removed,
            9.8,
        ),
        outcome(
            Kind::Cache,
            S::Caches,
            Some("/home/tstapler/.npm/_cacache"),
            Status::Removed,
            4.1,
        ),
        outcome(
            Kind::Cache,
            S::Caches,
            Some("/home/tstapler/.cache/pip"),
            Status::Removed,
            2.3,
        ),
        outcome(
            Kind::Worktree,
            S::Worktrees,
            Some("/home/tstapler/code/app/.claude/worktrees/agent-a41"),
            Status::Skipped(SkipReason::Dirty),
            1.2,
        ),
        outcome(
            Kind::NodeModules,
            S::BuildDirs,
            Some("/home/tstapler/old-app/node_modules"),
            Status::Error("permission denied (EACCES)".into()),
            21.7,
        ),
        outcome(
            Kind::SteamPrefix,
            S::Steam,
            Some("/home/tstapler/.steam/compat/440"),
            Status::Skipped(SkipReason::KindNotSelected),
            16.5,
        ),
        outcome(
            Kind::DockerVolume,
            S::DockerVolumes,
            None,
            Status::Skipped(SkipReason::KindNotSelected),
            11.0,
        ),
        outcome(
            Kind::DockerImage,
            S::Docker,
            None,
            Status::Skipped(SkipReason::KindNotSelected),
            5.0,
        ),
    ]
}

fn btrfs_at(avail: u64, data_used: u64) -> FsSnapshot {
    let mut s = fs_btrfs().snapshot;
    s.avail_bytes = avail;
    if let Some(b) = s.btrfs.as_mut() {
        b.data.bytes_used = data_used;
    }
    s
}

pub fn summary_of(outcomes: Vec<Outcome>, verdict: DiskVerdict) -> RunSummary {
    let before_used = (1.69 * TIB as f64) as u64;
    let after_used = before_used - gib(75.4);
    RunSummary::finish(FinishInput {
        fs_before: btrfs_at(gib(70.2), before_used),
        fs_after: Some(btrfs_at(gib(145.6), after_used)),
        settled: Some(Settled {
            stable: true,
            data_bytes_used: Some(after_used),
            samples: 2,
            waited_ms: 500,
        }),
        outcomes,
        extents: ExtentAttribution::Measured(Vec::new()),
        pre_delete: Vec::new(),
        delete_window_ms: 0,
        held: Vec::new(),
        snapshots: None,
        docker_delta_bytes: None,
        verdict_after: verdict,
        timings: StageTimings::default(),
        walker: WalkerCounters::default(),
        notes: Vec::new(),
    })
}

pub struct RunFx {
    pub summary: RunSummary,
    pub selection: Selection,
}

impl RunFx {
    pub fn s3() -> RunFx {
        RunFx::with(s3_outcomes(), DiskVerdict::Warn)
    }

    pub fn with(outcomes: Vec<Outcome>, verdict: DiskVerdict) -> RunFx {
        RunFx {
            summary: summary_of(outcomes, verdict),
            selection: selection_of(&[
                SelectorId::Caches,
                SelectorId::Worktrees,
                SelectorId::BuildDirs,
            ]),
        }
    }

    pub fn input(&self, exit: i32) -> ApplyInput<'_> {
        let mut labels = HashMap::new();
        for o in &self.summary.outcomes {
            if o.path.is_none() {
                labels.insert(o.id, OsString::from(format!("{} items", o.selector.id())));
            }
        }
        ApplyInput {
            plan_id: PlanId::from_hex("91be04d7").unwrap(),
            selection: &self.selection,
            gate_bypass: None,
            audit_path: Some("/home/tstapler/.local/state/dunnage/audit.jsonl".into()),
            selected_items: 7,
            selected_est: gib(96.4),
            last_dry_run: Some(dunnage_cli::apply::LastDryRun::Matches),
            summary: &self.summary,
            exit_code: exit,
            not_attempted: None,
            exe: EXE.into(),
            home: Some(HOME.into()),
            labels,
            facts: HashMap::new(),
            verbose: false,
            quiet: false,
            tty: false,
            marker_printed: false,
        }
    }
}

//! The per-gate kind registry (Story 2.3.4, Task 2.3.4b): which kinds exist in this binary and which of them
//! may delete. `ci/safety/kinds-by-gate.txt` is the table; `tests/kind_registry.rs` compares it with what
//! [`KindRegistry::build`] registers.
//!
//! Owner decision of 2026-11-10: the apply path of a gate is built before its owner review, but the kinds of
//! that gate stay unregistered in the shipped binary until the review. So the registry is driven by the build
//! gate, a cargo feature (`gate-b`, `gate-c`, `gate-d`; none is on by default), and a kind whose gate the
//! build has not reached is absent. Caches are the one kind that is also listed before it can delete: the
//! Gate A binary plans them, and `--apply` is refused because none of its kinds is apply-capable.

use std::path::PathBuf;

use disk_cleanup_adapters::targets::cache::CacheTarget;
use disk_cleanup_adapters::targets::fixture_kind::fixture_destructive_target;
use disk_cleanup_adapters::RealDirOps;
use disk_cleanup_core::roots::RootSet;
use disk_cleanup_core::target::Target;
use disk_cleanup_core::SelectorId;

mod btrfs;
mod build_dirs;
mod docker;
mod docker_images;
mod docker_volumes;
mod steam;
mod worktrees;

/// The gates in order; `Registrar::add` compares them as letters.
pub const GATES: [char; 4] = ['A', 'B', 'C', 'D'];

/// The gate this binary was built for: `A` unless a `gate-*` feature is on.
pub const fn build_gate() -> char {
    if cfg!(feature = "gate-d") {
        'D'
    } else if cfg!(feature = "gate-c") {
        'C'
    } else if cfg!(feature = "gate-b") {
        'B'
    } else {
        'A'
    }
}

/// What a slot needs to build its target.
pub struct KindCtx<'a> {
    pub roots: RootSet,
    /// Reads are confined to this directory (`DC_TEST_ROOT`, else `/`); absolute table rows live below it.
    pub fs_root: PathBuf,
    /// Opens the directory capability the targets delete through. A plan run passes a read-only confinement,
    /// an apply passes the confined or unconfined one of `wiring::confine`.
    pub ops: &'a (dyn Fn() -> RealDirOps + Send + Sync),
    /// `DC_TEST_ROOT` is set: the test-only kinds may be registered (only a `test-support` build has them).
    pub fixture_mode: bool,
}

/// One registered kind.
pub struct Registered {
    pub target: Box<dyn Target>,
    /// The selectors that can delete in this build.
    pub apply_capable: Vec<SelectorId>,
}

/// Collects the targets a slot adds, applying the build gate.
pub struct Registrar {
    gate: char,
    registered: Vec<Registered>,
}

impl Registrar {
    /// Registers `target` when the build has reached `listed_from`; its selectors can delete once the build
    /// has also reached `apply_from`.
    pub fn add(&mut self, listed_from: char, apply_from: char, target: Box<dyn Target>) {
        if self.gate < listed_from {
            return;
        }
        let apply_capable = if self.gate >= apply_from {
            target.selectors().to_vec()
        } else {
            Vec::new()
        };
        self.registered.push(Registered {
            target,
            apply_capable,
        });
    }
}

/// The targets of one build and the selectors among them that may delete.
pub struct KindRegistry {
    registered: Vec<Registered>,
}

impl KindRegistry {
    /// The registry of a build that reached `gate` (`A` to `D`).
    pub fn build_for(gate: char, ctx: &KindCtx<'_>) -> KindRegistry {
        let mut reg = Registrar {
            gate,
            registered: Vec::new(),
        };
        // Caches are listed from Gate A (plan only) and delete from Gate B.
        reg.add(
            'A',
            'B',
            Box::new(CacheTarget::new(
                ctx.roots.clone(),
                ctx.fs_root.clone(),
                (ctx.ops)(),
            )),
        );
        worktrees::register(&mut reg, ctx);
        build_dirs::register(&mut reg, ctx);
        docker::register(&mut reg, ctx);
        docker_images::register(&mut reg, ctx);
        docker_volumes::register(&mut reg, ctx);
        steam::register(&mut reg, ctx);
        btrfs::register(&mut reg, ctx);
        if ctx.fixture_mode {
            // Test-only: present in a `test-support` build under `DC_TEST_ROOT`, whatever the gate, and always
            // able to delete (it can touch only the fixture).
            if let Some(t) = fixture_destructive_target(ctx.roots.clone(), (ctx.ops)()) {
                reg.registered.push(Registered {
                    apply_capable: t.selectors().to_vec(),
                    target: t,
                });
            }
        }
        KindRegistry {
            registered: reg.registered,
        }
    }

    /// The registry of this binary.
    pub fn build(ctx: &KindCtx<'_>) -> KindRegistry {
        KindRegistry::build_for(build_gate(), ctx)
    }

    pub fn targets(&self) -> Vec<&dyn Target> {
        self.registered.iter().map(|r| r.target.as_ref()).collect()
    }

    /// Every selector some registered target serves.
    pub fn listed(&self) -> Vec<SelectorId> {
        let mut v: Vec<SelectorId> = self
            .registered
            .iter()
            .flat_map(|r| r.target.selectors().iter().copied())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// The selectors that may delete in this build; empty for the shipped binary until Gate B's review.
    pub fn apply_capable(&self) -> Vec<SelectorId> {
        let mut v: Vec<SelectorId> = self
            .registered
            .iter()
            .flat_map(|r| r.apply_capable.iter().copied())
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

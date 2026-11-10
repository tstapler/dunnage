//! Composition root pieces of Story 2.2.1: the baked build provenance, the audit sink and plan store over a
//! state dir, and the gate-stamp check that an unconfined `--apply` passes (or breaks glass through) before the
//! plan-id gate, the lock and the audit log.

use std::io;
use std::path::Path;
use std::sync::Arc;

use disk_cleanup_adapters::audit::RealAuditSink;
use disk_cleanup_adapters::gate_stamp::{
    break_glass, current_exe_sha256, open_controlling_tty, read_stamp, stamp_path,
    BreakGlassRefusal,
};
use disk_cleanup_adapters::state::{RealPlanStore, StateDir};
use disk_cleanup_core::{
    break_glass_warning, check_stamp, BreakGlass, BuildInfo, GateStamp, StampRefusal, StampSummary,
    BREAK_GLASS_NO_TTY, BREAK_GLASS_NO_TTY_CODE,
};

/// What `cli/build.rs` baked in. `patched` is always false since the scanner is a workspace member (ADR-007);
/// `metascope_rev` is the last commit that touched `crates/scan`.
pub fn build_info() -> BuildInfo {
    BuildInfo {
        bin_version: env!("CARGO_PKG_VERSION").to_string(),
        git_commit: env!("DC_GIT_COMMIT").to_string(),
        dirty: env!("DC_GIT_DIRTY") == "true",
        metascope_rev: env!("DC_SCAN_REV").to_string(),
        patched: false,
        metascope_dirty: env!("DC_SCAN_DIRTY") == "true",
    }
}

pub fn audit_sink(state: &StateDir) -> RealAuditSink {
    RealAuditSink::new(
        state.paths.audit(),
        state.paths.audit_reserve(),
        state.owner,
    )
}

pub fn plan_store(state: StateDir) -> RealPlanStore {
    let now: Arc<dyn Fn() -> i64 + Send + Sync> = Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    });
    RealPlanStore::new(state, now)
}

/// An exit-64 refusal of the stamp step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateExit {
    pub text: String,
    /// The `code` of the `--json` error document.
    pub json_code: &'static str,
}

/// Usage-class exit code of every refusal of the stamp step.
pub const GATE_EXIT_CODE: i32 = 64;

/// How the stamp step ended for an apply that may go on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StampOutcome {
    /// Confinement mode (the fixture binary): the run can touch only the fixture, so no stamp is needed.
    Confined,
    Verified(StampSummary),
    /// A person typed BYPASS; `warning` is the line directly under L1.
    BrokeGlass {
        break_glass: BreakGlass,
        warning: String,
    },
}

pub struct StampInputs<'a> {
    pub build: &'a BuildInfo,
    pub stamp: Option<&'a GateStamp>,
    pub exe_sha256: &'a str,
    pub required_gate: char,
    pub confined: bool,
    /// `DISK_CLEANUP_UNGATED`.
    pub ungated_var: Option<&'a str>,
}

/// The stamp step of an unconfined `--apply`. `open_tty` opens the controlling terminal (never stdin).
pub fn stamp_gate(
    inp: &StampInputs<'_>,
    open_tty: &dyn Fn() -> io::Result<std::fs::File>,
) -> Result<StampOutcome, GateExit> {
    if inp.confined {
        return Ok(StampOutcome::Confined);
    }
    let refusal = match check_stamp(inp.build, inp.stamp, inp.exe_sha256, inp.required_gate) {
        Ok(summary) => return Ok(StampOutcome::Verified(summary)),
        Err(r) => r,
    };
    match break_glass(inp.ungated_var, open_tty) {
        Ok(bg) => {
            let warning = break_glass_warning(&bg);
            Ok(StampOutcome::BrokeGlass {
                break_glass: bg,
                warning,
            })
        }
        Err(BreakGlassRefusal::NoTerminal) => Err(GateExit {
            text: BREAK_GLASS_NO_TTY.to_string(),
            json_code: BREAK_GLASS_NO_TTY_CODE,
        }),
        Err(BreakGlassRefusal::NotRequested | BreakGlassRefusal::NotConfirmed) => {
            Err(refusal_exit(&refusal))
        }
    }
}

fn refusal_exit(r: &StampRefusal) -> GateExit {
    GateExit {
        text: r.text(),
        json_code: r.json_code(),
    }
}

/// The production step: the installed stamp in the state dir, the hash of this executable, the real terminal.
pub fn real_stamp_gate(
    state_dir: &Path,
    build: &BuildInfo,
    required_gate: char,
    confined: bool,
    ungated_var: Option<&str>,
) -> Result<StampOutcome, GateExit> {
    if confined {
        return Ok(StampOutcome::Confined);
    }
    let stamp = read_stamp(&stamp_path(state_dir));
    let exe_sha256 = current_exe_sha256().unwrap_or_default();
    stamp_gate(
        &StampInputs {
            build,
            stamp: stamp.as_ref(),
            exe_sha256: &exe_sha256,
            required_gate,
            confined,
            ungated_var,
        },
        &open_controlling_tty,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
mod tests {
    use super::*;
    use std::io::Write;

    const COMMIT: &str = "c0ffee1c0ffee1c0ffee1c0ffee1c0ffee1c0ffe";
    const SHA: &str = "ab12";

    fn build() -> BuildInfo {
        BuildInfo {
            bin_version: "0.2.0".into(),
            git_commit: COMMIT.into(),
            dirty: false,
            metascope_rev: COMMIT.into(),
            patched: false,
            metascope_dirty: false,
        }
    }

    fn stamp() -> GateStamp {
        GateStamp {
            schema: 1,
            gate: "B".into(),
            commit: COMMIT.into(),
            binary_sha256: SHA.into(),
        }
    }

    fn inputs<'a>(
        b: &'a BuildInfo,
        s: Option<&'a GateStamp>,
        var: Option<&'a str>,
        confined: bool,
    ) -> StampInputs<'a> {
        StampInputs {
            build: b,
            stamp: s,
            exe_sha256: SHA,
            required_gate: 'B',
            confined,
            ungated_var: var,
        }
    }

    fn no_tty() -> io::Result<std::fs::File> {
        Err(io::Error::from_raw_os_error(6))
    }

    /// A socket pair standing in for the terminal: reads and writes have separate buffers, as on a tty.
    fn typed(text: &str) -> impl Fn() -> io::Result<std::fs::File> {
        use std::os::unix::net::UnixStream;
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        theirs.write_all(text.as_bytes()).unwrap();
        let slot = std::sync::Mutex::new(Some((ours, theirs)));
        move || {
            let (ours, theirs) = slot.lock().unwrap().take().expect("opened once");
            // Keep the typing end open until the read is done by leaking it to the process end.
            std::mem::forget(theirs);
            Ok(std::fs::File::from(std::os::fd::OwnedFd::from(ours)))
        }
    }

    #[test]
    fn stamp_gate_should_skip_when_confined_accept_a_match_and_refuse_with_exit_64_otherwise() {
        let b = build();
        let s = stamp();
        assert_eq!(
            stamp_gate(&inputs(&b, None, None, true), &no_tty).unwrap(),
            StampOutcome::Confined
        );
        assert!(matches!(
            stamp_gate(&inputs(&b, Some(&s), None, false), &no_tty).unwrap(),
            StampOutcome::Verified(v) if v.verified
        ));
        let e = stamp_gate(&inputs(&b, None, None, false), &no_tty).unwrap_err();
        assert_eq!(e.json_code, "gate_stamp");
        assert!(
            e.text.contains("no stamp was installed") && e.text.ends_with("Nothing was deleted.")
        );
        // An empty variable is as if unset: the stamp refusal, not the terminal.
        let e = stamp_gate(&inputs(&b, None, Some(""), false), &no_tty).unwrap_err();
        assert_eq!(e.json_code, "gate_stamp");
    }

    #[test]
    fn break_glass_without_a_terminal_should_refuse_with_break_glass_no_tty() {
        let b = build();
        let e = stamp_gate(&inputs(&b, None, Some("full disk"), false), &no_tty).unwrap_err();
        assert_eq!(e.json_code, "break_glass_no_tty");
        assert_eq!(e.text, BREAK_GLASS_NO_TTY);
        assert_eq!(GATE_EXIT_CODE, 64);
    }

    #[test]
    fn break_glass_should_proceed_only_when_the_word_is_typed_on_the_terminal() {
        let b = build();
        let ok = stamp_gate(
            &inputs(&b, None, Some("full disk"), false),
            &typed("BYPASS\n"),
        )
        .unwrap();
        let StampOutcome::BrokeGlass {
            break_glass,
            warning,
        } = ok
        else {
            panic!("expected a bypass")
        };
        assert!(break_glass.tty_confirmed);
        assert_eq!(warning, "WARNING: gate check bypassed (full disk)");
        let e =
            stamp_gate(&inputs(&b, None, Some("full disk"), false), &typed("yes\n")).unwrap_err();
        assert_eq!(e.json_code, "gate_stamp");
    }

    #[test]
    fn build_info_should_carry_the_baked_commit_and_a_package_version() {
        let b = build_info();
        assert_eq!(b.bin_version, env!("CARGO_PKG_VERSION"));
        assert!(!b.git_commit.is_empty() && !b.metascope_rev.is_empty());
        assert!(!b.patched);
    }
}

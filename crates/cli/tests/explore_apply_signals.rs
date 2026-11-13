#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cloned_ref_to_slice_refs,
    clippy::redundant_closure_for_method_calls,
    non_snake_case
)]
//! Task 6.6.3b, review 2 N8: SIGINT, SIGTERM, SIGHUP and SIGQUIT delivered in the middle of the second item.
//! The handler records the signal, the apply's bridge sets the run guard's `AbortFlag`, the tree stops at its
//! next entry, the log shows an `intent` with its `result` and a `run_end ... interrupted`, and the exit code is
//! 130 (SIGINT) or 143. The signal handlers are process-wide and record only the first signal, so each signal
//! runs in its own child process (this test binary, re-run by name).

mod explore_rig;

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use dunnage_adapters::signals::install_term_handlers;
use dunnage_adapters::testing::{Event, FaultDirOps};
use dunnage_adapters::RealDirOps;
use dunnage_core::AuditRecord;
use explore_rig::{kinds_of, Ex};

const VAR: &str = "DC_EX_SIGNAL";

fn parent(sig: &str, expect_exit: i32) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_signal_mid_apply", "--nocapture"])
        .env(VAR, format!("{sig}:{expect_exit}"))
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("CHILD_OK"), "signal {sig}: {text}");
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "explorer delete fails closed without the kernel mount id (statx STATX_MNT_ID): Linux only"
)]
fn safety_apply_should_stop_at_the_next_entry_and_exit_130_When_sigint_arrives_mid_item() {
    parent("INT", 130);
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "explorer delete fails closed without the kernel mount id (statx STATX_MNT_ID): Linux only"
)]
fn safety_apply_should_stop_at_the_next_entry_and_exit_143_When_sigterm_arrives_mid_item() {
    parent("TERM", 143);
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "explorer delete fails closed without the kernel mount id (statx STATX_MNT_ID): Linux only"
)]
fn safety_apply_should_stop_at_the_next_entry_and_exit_143_When_sighup_arrives_mid_item() {
    parent("HUP", 143);
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "explorer delete fails closed without the kernel mount id (statx STATX_MNT_ID): Linux only"
)]
fn safety_apply_should_stop_at_the_next_entry_and_exit_143_When_sigquit_arrives_mid_item() {
    parent("QUIT", 143);
}

#[test]
fn child_signal_mid_apply() {
    let Ok(spec) = std::env::var(VAR) else {
        return;
    };
    let (sig, expect) = spec.split_once(':').unwrap();
    let (sig, expect): (String, i32) = (sig.to_string(), expect.parse().unwrap());
    install_term_handlers().unwrap();

    let ex = Ex::new();
    let a = ex.tree("app/a1", 3, 300);
    let b = ex.tree("app/b2", 6, 300);
    let c = ex.tree("app/c3", 2, 300);
    let confirmed = ex.confirmed(&[a.clone(), b.clone(), c.clone()]);
    let hits = Arc::new(AtomicUsize::new(0));
    let sig = sig.clone();
    let ops = |_: &dunnage_core::ApplyToken| {
        let hits = Arc::clone(&hits);
        FaultDirOps::wrap(RealDirOps::confined_to(&ex.base))
            .hook(Event::BeforeUnlinkat, {
                let sig = sig.clone();
                move |ctx| {
                    let in_b2 = ctx.base_abs.to_string_lossy().contains("b2")
                        || ctx.rel_path.to_string_lossy().contains("b2");
                    if in_b2 && hits.fetch_add(1, Ordering::SeqCst) == 1 {
                        // Delivered to this process; the handler only stores into atomics.
                        let st = Command::new("kill")
                            .args(["-s", &sig, &std::process::id().to_string()])
                            .status()
                            .unwrap();
                        assert!(st.success());
                        // Block as a slow filesystem would, until the bridge has set the flag.
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                }
            })
            .build()
    };
    let ran = ex.run(confirmed, ops);

    let kinds = kinds_of(&ex.audit);
    assert_eq!(kinds.last(), Some(&"run_end"), "{kinds:?}");
    let intents = kinds.iter().filter(|k| **k == "intent").count();
    let results = kinds.iter().filter(|k| **k == "result").count();
    assert!(intents >= 2 && results >= intents, "{kinds:?}");
    assert!(ran.report.summary.interrupted);
    assert_eq!(ran.report.exit, expect);
    assert!(!a.exists(), "the first tree was removed whole");
    assert!(b.exists(), "the second tree stopped part way");
    assert!(c.exists(), "the third was never started");
    let end = ex
        .audit
        .records()
        .into_iter()
        .find_map(|r| match r {
            AuditRecord::RunEnd(e) => Some(e),
            _ => None,
        })
        .unwrap();
    assert!(end.interrupted);
    println!("CHILD_OK");
}

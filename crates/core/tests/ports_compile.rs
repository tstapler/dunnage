#![allow(non_snake_case)]
//! Every port is `Send + Sync`: one `&Env` is shared across scoped worker threads. The `compile_fail` half
//! (no `run_mutating` without a token) is a doctest on `CommandRunner` in `core/src/ports.rs`.

use dunnage_core::ports::Env;
use dunnage_core::testing::{
    FakeAuditSink, FakeClock, FakeCommandRunner, FakeFs, FakePlanStore, FakeProbe, FakeRunGuard,
    FakeSpaceMeter,
};
use dunnage_core::Dirs;

#[test]
fn env_should_be_shareable_across_scoped_threads_When_built_from_the_fakes() {
    let fs = FakeFs::new();
    let runner = FakeCommandRunner::new();
    let clock = FakeClock::at(1);
    let probe = FakeProbe::new();
    let space = FakeSpaceMeter::new(vec![], FakeSpaceMeter::stable());
    let audit = FakeAuditSink::new();
    let guard = FakeRunGuard::new();
    let plans = FakePlanStore::new();
    let env = Env {
        fs: &fs,
        runner: &runner,
        clock: &clock,
        probe: &probe,
        space: &space,
        audit: &audit,
        guard: &guard,
        plans: &plans,
        dirs: Dirs {
            home: "/h".into(),
            state: "/h/s".into(),
            cache: "/h/c".into(),
            config: "/h/f".into(),
        },
    };
    let env = &env;
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(move || {
                assert_eq!(env.clock.now_epoch(), 1);
                assert_eq!(env.probe.euid(), 1000);
            });
        }
    });
}

#[test]
fn every_port_trait_should_be_send_and_sync() {
    use dunnage_core::ports::{
        AuditSink, Clock, CommandRunner, Fs, PlanStore, RunGuard, SpaceMeter, SystemProbe,
    };
    fn ok<T: Send + Sync + ?Sized>() {}
    ok::<dyn CommandRunner>();
    ok::<dyn Fs>();
    ok::<dyn Clock>();
    ok::<dyn SystemProbe>();
    ok::<dyn SpaceMeter>();
    ok::<dyn AuditSink>();
    ok::<dyn RunGuard>();
    ok::<dyn PlanStore>();
}

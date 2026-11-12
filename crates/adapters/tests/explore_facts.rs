#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.3.1 Task 6.3.1d: the classification-facts loader over a fixture Steam library and a scripted
//! `CommandRunner`.

mod common;
use common::discovery::{fixture, roots_for};

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dunnage_adapters::explore::facts::ClassifyFactsLoader;
use dunnage_core::delete_types::DeleteError;
use dunnage_core::explore::msg::ScanMsg;
use dunnage_core::ids::AppId;
use dunnage_core::ports::{CmdOutput, CommandRunner};
use dunnage_core::testing::FakeCommandRunner;

/// `libraryfolders.vdf`, `appmanifest_620.acf`, `compatdata/{620,999}` and `common/OldGame`.
fn steam_fixture(base: &Path) -> PathBuf {
    let root = base.join("Steam");
    fs::create_dir_all(root.join("steamapps/compatdata/620")).unwrap();
    fs::create_dir_all(root.join("steamapps/compatdata/999")).unwrap();
    fs::create_dir_all(root.join("steamapps/common/OldGame")).unwrap();
    fs::create_dir_all(root.join("steamapps/common/Portal 2")).unwrap();
    let mut vdf = String::from("\"libraryfolders\"\n{\n");
    write!(
        vdf,
        "\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t\t\"label\"\t\t\"\"\n\t\t\"apps\"\n\t\t{{\n\t\t}}\n\t}}\n}}\n",
        root.to_str().unwrap()
    )
    .unwrap();
    fs::write(root.join("steamapps/libraryfolders.vdf"), vdf).unwrap();
    fs::write(
        root.join("steamapps/appmanifest_620.acf"),
        "\"AppState\"\n{\n\t\"appid\"\t\t\"620\"\n\t\"name\"\t\t\"Portal 2\"\n\t\"installdir\"\t\t\"Portal 2\"\n\t\"StateFlags\"\t\t\"4\"\n}\n",
    )
    .unwrap();
    root
}

fn runner_with_go(reported: Option<&Path>) -> Arc<dyn CommandRunner> {
    let r = FakeCommandRunner::new();
    r.absent("pip").absent("pip3");
    match reported {
        Some(p) => r.on(
            &["go", "env", "GOCACHE"],
            Ok(CmdOutput {
                stdout: format!("{}\n", p.display()).into_bytes(),
                stderr: Vec::new(),
            }),
        ),
        None => r.on(
            &["go", "env", "GOCACHE"],
            Err(DeleteError::CommandFailed {
                status: 2,
                message: "go: not found".into(),
            }),
        ),
    };
    Arc::new(r)
}

#[test]
fn loader_should_send_installed_ids_install_dirs_and_resolved_cache_paths_When_run_on_its_own_thread(
) {
    let f = fixture();
    let steam = steam_fixture(f.home.parent().unwrap());
    let relocated = f.home.join(".cache/custom/go-build");
    let loader = ClassifyFactsLoader::new(
        roots_for(&f),
        vec![steam.clone()],
        runner_with_go(Some(&relocated)),
    );
    let (tx, rx) = channel();
    let t = Instant::now();
    let handle = loader.spawn(tx).unwrap();
    let ScanMsg::Facts(facts) = rx.recv_timeout(Duration::from_secs(10)).unwrap() else {
        panic!("not Facts")
    };
    handle.join().unwrap();
    assert!(t.elapsed() < Duration::from_secs(10));

    let libs = facts.steam.expect("Steam facts loaded");
    assert_eq!(libs.len(), 1);
    assert_eq!(libs[0].root, steam.canonicalize().unwrap());
    assert!(libs[0].mounted);
    assert_eq!(libs[0].installed, vec![AppId(620)]);
    assert_eq!(
        libs[0].install_dirs,
        vec![std::ffi::OsString::from("Portal 2")]
    );

    assert!(
        facts.cache_paths.contains(&relocated),
        "{:?}",
        facts.cache_paths
    );
    assert!(
        facts.cache_paths.contains(&f.home.join(".npm/_cacache")),
        "{:?}",
        facts.cache_paths
    );
}

#[test]
fn loader_should_leave_steam_absent_and_keep_table_paths_When_libraryfolders_is_unreadable_and_go_env_fails(
) {
    let f = fixture();
    let steam = steam_fixture(f.home.parent().unwrap());
    fs::write(steam.join("steamapps/libraryfolders.vdf"), "{ not vdf").unwrap();
    let loader = ClassifyFactsLoader::new(roots_for(&f), vec![steam], runner_with_go(None));
    let facts = loader.load();
    assert!(
        facts.steam.is_none(),
        "an unreadable library list leaves Steam rows at Shape"
    );
    assert!(!facts.cache_paths.is_empty(), "the table paths remain");
    assert!(!facts.cache_paths.iter().any(|p| p.ends_with("go-build")));
}

#[test]
fn loader_should_refuse_a_relocated_cache_the_roots_do_not_admit() {
    let f = fixture();
    let outside = PathBuf::from("/var/tmp/go-build");
    let facts =
        ClassifyFactsLoader::new(roots_for(&f), vec![], runner_with_go(Some(&outside))).load();
    assert!(!facts.cache_paths.contains(&outside));
    assert_eq!(
        facts.steam,
        Some(vec![]),
        "no Steam install is a loaded, empty set"
    );
}

#[test]
fn loader_should_be_silent_about_a_missing_steam_root() {
    let f = fixture();
    let facts = ClassifyFactsLoader::new(
        roots_for(&f),
        vec![f.home.join("no-such-steam")],
        runner_with_go(None),
    )
    .load();
    assert!(facts.steam.is_none());
}

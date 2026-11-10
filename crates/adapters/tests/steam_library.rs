#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Steam library discovery over generated fixtures (Story 4.2.1). Tempdirs only, never the real Steam root.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use disk_cleanup_adapters::steam::{default_steam_roots, plan_gate, SteamGate, SteamInstall};
use disk_cleanup_core::testing::FakeProbe;
use disk_cleanup_core::{Platform, SkipReason, Tri};

/// A Steam root plus `n` library directories, all inside one tempdir.
struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    libs: Vec<PathBuf>,
}

fn vdf_escape(p: &Path) -> String {
    p.to_str().unwrap().replace('\\', "\\\\")
}

fn write_folders(root: &Path, libs: &[PathBuf]) {
    let mut s = String::from("\"libraryfolders\"\n{\n");
    for (i, l) in libs.iter().enumerate() {
        write!(s,
            "\t\"{i}\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t\t\"label\"\t\t\"\"\n\t\t\"apps\"\n\t\t{{\n\t\t}}\n\t}}\n",
            vdf_escape(l)
        )
        .unwrap();
    }
    s.push_str("}\n");
    fs::write(root.join("steamapps/libraryfolders.vdf"), s).unwrap();
}

fn write_manifest(lib: &Path, appid: u32, installdir: &str) {
    let body = format!(
        "\"AppState\"\n{{\n\t\"appid\"\t\t\"{appid}\"\n\t\"name\"\t\t\"Game {appid}\"\n\t\"installdir\"\t\t\"{installdir}\"\n\t\"StateFlags\"\t\t\"4\"\n}}\n"
    );
    fs::write(lib.join(format!("steamapps/appmanifest_{appid}.acf")), body).unwrap();
}

/// `lib_names` are directory names under the tempdir; the first library is the Steam root itself.
fn fixture(lib_names: &[&str]) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("Steam");
    let mut libs = vec![root.clone()];
    libs.extend(lib_names.iter().map(|n| base.join(n)));
    for l in &libs {
        fs::create_dir_all(l.join("steamapps")).unwrap();
    }
    write_folders(&root, &libs);
    Fixture {
        _tmp: tmp,
        root,
        libs,
    }
}

fn two_library_fixture() -> Fixture {
    let f = fixture(&["lib1"]);
    write_manifest(&f.libs[0], 620, "Portal 2");
    write_manifest(&f.libs[1], 1_091_500, "Cyberpunk 2077");
    f
}

#[test]
fn installed_set_should_be_union_620_and_1091500_When_two_libraries() {
    let f = two_library_fixture();
    let install = SteamInstall::load(&f.root).unwrap();
    assert_eq!(
        install.installed_ids().into_iter().collect::<Vec<_>>(),
        vec![620, 1_091_500]
    );
    assert_eq!(install.libraries, f.libs);
}

#[test]
fn installed_set_should_record_installdir_and_library_matched_case_insensitively_When_manifests_read(
) {
    let f = two_library_fixture();
    let install = SteamInstall::load(&f.root).unwrap();
    let portal = install.app_for_dir("portal 2".as_ref()).unwrap();
    assert_eq!(
        (portal.appid, portal.installdir.as_str()),
        (620, "Portal 2")
    );
    assert_eq!(portal.library, f.libs[0]);
    let cp = install.app_for_dir("CYBERPUNK 2077".as_ref()).unwrap();
    assert_eq!((cp.appid, &cp.library), (1_091_500, &f.libs[1]));
    assert!(install.app_for_dir("OldGame".as_ref()).is_none());
}

#[test]
fn unmounted_library_should_skip_whole_kind_LibraryUnmounted_When_lib1_missing() {
    let f = two_library_fixture();
    fs::remove_dir_all(&f.libs[1]).unwrap();
    let probe = FakeProbe::new();
    assert_eq!(
        plan_gate(&probe, &f.root).unwrap_err(),
        SteamGate::Skip(SkipReason::LibraryUnmounted)
    );
}

#[test]
fn steam_running_or_downloading_nonempty_should_skip_SteamRunning_When_planned() {
    let f = two_library_fixture();
    let running = FakeProbe::new().process("steam", Tri::Running);
    assert_eq!(
        plan_gate(&running, &f.root).unwrap_err(),
        SteamGate::Skip(SkipReason::SteamRunning)
    );
    let unknown = FakeProbe::new().process("steam", Tri::Unknown);
    assert_eq!(
        plan_gate(&unknown, &f.root).unwrap_err(),
        SteamGate::Skip(SkipReason::SteamRunning)
    );

    let idle = FakeProbe::new();
    assert!(plan_gate(&idle, &f.root).is_ok());
    let dl = f.libs[1].join("steamapps/downloading");
    fs::create_dir_all(&dl).unwrap();
    assert!(
        plan_gate(&idle, &f.root).is_ok(),
        "an empty downloading dir is fine"
    );
    fs::create_dir(dl.join("620")).unwrap();
    assert_eq!(
        plan_gate(&idle, &f.root).unwrap_err(),
        SteamGate::Skip(SkipReason::SteamRunning)
    );
}

#[test]
fn steam_started_between_plan_and_execute_should_fail_the_gate_When_rechecked() {
    let f = two_library_fixture();
    assert!(plan_gate(&FakeProbe::new(), &f.root).is_ok());
    let started = FakeProbe::new().process("steam", Tri::Running);
    assert_eq!(
        plan_gate(&started, &f.root).unwrap_err(),
        SteamGate::Skip(SkipReason::SteamRunning)
    );
}

#[test]
fn library_paths_with_spaces_and_vdf_escapes_should_parse_When_libraryfolders_vdf() {
    let f = fixture(&["my games", "Steam Library \\ D"]);
    for (i, l) in f.libs.iter().enumerate().skip(1) {
        write_manifest(l, 100 + i as u32, "G");
    }
    let raw = fs::read_to_string(f.root.join("steamapps/libraryfolders.vdf")).unwrap();
    assert!(
        raw.contains("\\\\"),
        "fixture must hold an escaped backslash"
    );
    let install = SteamInstall::load(&f.root).unwrap();
    assert_eq!(install.libraries, f.libs);
    assert_eq!(
        install.installed_ids().into_iter().collect::<Vec<_>>(),
        vec![101, 102]
    );
}

#[test]
fn unparseable_or_empty_vdf_acf_should_make_steam_Unavailable_UnrecognisedOutput_and_an_empty_installed_set_with_compatdata_entries_should_be_LibraryUnmounted_When_fixtures_mutated(
) {
    let unrecognised = Err(SteamGate::Unavailable(SkipReason::UnrecognisedOutput));
    let vdf = |f: &Fixture| f.root.join("steamapps/libraryfolders.vdf");

    // The unmutated fixture parses.
    assert!(SteamInstall::load(&two_library_fixture().root).is_ok());

    // Zero libraries.
    let f = two_library_fixture();
    fs::write(vdf(&f), "\"libraryfolders\"\n{\n}\n").unwrap();
    assert_eq!(SteamInstall::load(&f.root).map(|_| ()), unrecognised);

    // Truncated.
    let f = two_library_fixture();
    let full = fs::read_to_string(vdf(&f)).unwrap();
    fs::write(vdf(&f), &full[..full.len() / 2]).unwrap();
    assert_eq!(SteamInstall::load(&f.root).map(|_| ()), unrecognised);

    // An entry without `path`.
    let f = two_library_fixture();
    fs::write(
        vdf(&f),
        "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"label\"\t\"x\"\n\t}\n}\n",
    )
    .unwrap();
    assert_eq!(SteamInstall::load(&f.root).map(|_| ()), unrecognised);

    // Missing file.
    let f = two_library_fixture();
    fs::remove_file(vdf(&f)).unwrap();
    assert_eq!(SteamInstall::load(&f.root).map(|_| ()), unrecognised);

    // A manifest without appid, and one without installdir.
    for dropped in ["appid", "installdir"] {
        let f = two_library_fixture();
        let m = f.libs[0].join("steamapps/appmanifest_620.acf");
        let body: String = fs::read_to_string(&m)
            .unwrap()
            .lines()
            .filter(|l| !l.contains(dropped))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&m, body).unwrap();
        assert_eq!(
            SteamInstall::load(&f.root).map(|_| ()),
            unrecognised,
            "without {dropped}"
        );
    }

    // Empty installed set with compatdata entries: not every prefix is an orphan.
    let f = fixture(&[]);
    let install = SteamInstall::load(&f.root).unwrap();
    assert!(install.installed.is_empty());
    assert_eq!(
        install.check_against_compatdata(3),
        Err(SteamGate::Unavailable(SkipReason::LibraryUnmounted))
    );
    assert_eq!(install.check_against_compatdata(0), Ok(()));
}

#[test]
fn default_steam_roots_should_find_native_and_flatpak_without_duplicates_When_both_exist() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
    let native = home.join(".local/share/Steam");
    let flatpak = home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam");
    fs::create_dir_all(&native).unwrap();
    fs::create_dir_all(&flatpak).unwrap();
    fs::create_dir_all(home.join(".steam")).unwrap();
    std::os::unix::fs::symlink(&native, home.join(".steam/steam")).unwrap();
    assert_eq!(
        default_steam_roots(&home, Platform::Linux),
        vec![native, flatpak]
    );
    assert!(default_steam_roots(&home, Platform::Macos).is_empty());
}

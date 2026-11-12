#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 6.4.2, Task 6.4.2b: `--only PATH` through the spawned binary under `fixture_cmd()` (X-040).
//! The destructive-flow cases run on the test-only `fixture-destructive` kind (`stale-*` directories below
//! the scan roots) and, with `gate-c`, on `build-dirs`. Everything lives in a tempdir fixture HOME.

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;

use common::{aged_dir, extract_plan_id, next_commands, run_line, text_of, Fixture};

fn plain(f: &Fixture, args: &[&str]) -> Output {
    f.fixture_cmd()
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn root_with(f: &Fixture, names: &[&str]) -> PathBuf {
    let root = f.home().join("code/proj");
    for n in names {
        aged_dir(&root.join(n), 9_000, 90);
    }
    root
}

fn usage_error(out: &Output) -> String {
    let (stdout, stderr) = text_of(out);
    assert_eq!(out.status.code(), Some(64), "{stderr}");
    assert!(stdout.is_empty(), "stdout must be empty: {stdout}");
    assert!(stderr.ends_with("Nothing was deleted.\n"), "{stderr}");
    stderr
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", text_of(out).0))
}

fn paths(doc: &serde_json::Value) -> Vec<String> {
    doc["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["status"] == "would_free")
        .map(|i| i["path"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_malformed_only_value_should_exit_64_before_any_other_check() {
    let f = Fixture::new();
    let long = format!("/{}", "x".repeat(600));
    let too_many: Vec<String> = (0..21)
        .flat_map(|i| ["--only".to_string(), format!("/a/n{i}")])
        .collect();
    let cases: Vec<(Vec<String>, &str)> = vec![
        (
            vec!["--only".into(), "rel/path".into()],
            "not an absolute path",
        ),
        (vec!["--only".into(), String::new()], "it is empty"),
        (vec!["--only".into(), "/a/../b".into()], "'..' component"),
        (vec!["--only".into(), "/a/./b".into()], "'.' component"),
        (vec!["--only".into(), "/a//b".into()], "empty component"),
        (vec!["--only".into(), "/a/b/".into()], "ends with a slash"),
        (vec!["--only".into(), long], "longer than 512 bytes"),
        (too_many, "too many values"),
        // A value error wins over a flag that needs a kind that is not selected.
        (
            vec![
                "--only".into(),
                "rel".into(),
                "--docker-volumes".into(),
                "named".into(),
                "--kinds".into(),
                "caches".into(),
            ],
            "not an absolute path",
        ),
    ];
    for (args, needle) in cases {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let err = usage_error(&plain(&f, &refs));
        assert!(err.contains(needle), "{needle}: {err}");
        assert!(err.starts_with("error: "), "{err}");
    }
}

#[test]
fn a_non_item_should_exit_64_with_the_not_an_item_text_in_plan_and_apply_mode() {
    let f = Fixture::new();
    let root = root_with(&f, &["stale-a"]);
    let ghost = root.join("stale-ghost");
    for apply in [false, true] {
        let mut args = vec!["--kinds", "fixture-destructive", "--root"];
        let r = root.to_str().unwrap();
        args.push(r);
        args.push("--only");
        args.push(ghost.to_str().unwrap());
        if apply {
            args.push("--apply");
        }
        let err = usage_error(&plain(&f, &args));
        // The first run of a fresh fixture also prints the reserve-files note.
        let at = err.find("error: --only").unwrap_or_else(|| panic!("{err}"));
        assert!(
            err[at..].starts_with(&format!(
                "error: --only {} is not an item of the selected kinds (fixture-destructive). It may be new,",
                ghost.display()
            )),
            "{err}"
        );
    }
    // Every spelling that is not the walker's path is refused the same way.
    let a = root.join("stale-a");
    for spelling in [
        format!("{}/", a.display()),
        format!("{}//stale-a", root.display()),
        format!("{}/./stale-a", root.display()),
        format!("{}", root.display()),
    ] {
        let out = plain(
            &f,
            &[
                "--kinds",
                "fixture-destructive",
                "--root",
                root.to_str().unwrap(),
                "--only",
                &spelling,
            ],
        );
        assert_eq!(
            out.status.code(),
            Some(64),
            "{spelling}: {}",
            text_of(&out).1
        );
    }
}

#[test]
fn only_should_restrict_the_plan_change_the_plan_id_and_ride_last_on_the_next_lines() {
    let f = Fixture::new();
    let root = root_with(&f, &["stale-a", "stale-b", "stale-c"]);
    let r = root.to_str().unwrap();
    let base = ["--kinds", "fixture-destructive", "--root", r];
    let all = json(&plain(&f, &[&base[..], &["--json"]].concat()));
    assert_eq!(paths(&all).len(), 3);

    let b = root.join("stale-b");
    let one = json(&plain(
        &f,
        &[&base[..], &["--only", b.to_str().unwrap(), "--json"]].concat(),
    ));
    assert_eq!(paths(&one), vec![b.to_str().unwrap().to_string()]);
    assert_eq!(
        one["items"].as_array().unwrap().len(),
        1,
        "no FREE and no RISK row for the others"
    );
    assert_ne!(one["plan_id"], all["plan_id"]);
    assert_eq!(
        one["selection"]["only"],
        serde_json::json!([b.to_str().unwrap()])
    );
    assert_eq!(all["selection"]["only"], serde_json::json!([]));
    let cmds: Vec<String> = one["next"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["command"].as_str().unwrap().to_string())
        .collect();
    let apply = cmds
        .iter()
        .find(|c| c.contains("--apply"))
        .expect("an apply line");
    let tail = format!(
        "--root {r} --only {} --plan-id {}",
        b.display(),
        one["plan_id"].as_str().unwrap()
    );
    assert!(apply.ends_with(&tail), "{apply}");
}

#[test]
fn pasted_next_line_should_apply_and_a_destructive_line_minus_one_only_should_be_changed_while_a_safe_tier_line_minus_only_is_a_filter_that_the_hand_off_states(
) {
    let f = Fixture::new();
    let root = root_with(&f, &["stale-a", "stale-b", "stale-c"]);
    let r = root.to_str().unwrap();
    let (a, b) = (root.join("stale-a"), root.join("stale-b"));
    let dry = plain(
        &f,
        &[
            "--kinds",
            "fixture-destructive",
            "--root",
            r,
            "--only",
            a.to_str().unwrap(),
            "--only",
            b.to_str().unwrap(),
        ],
    );
    assert_eq!(dry.status.code(), Some(0), "{}", text_of(&dry).1);
    let (stdout, _) = text_of(&dry);
    let line = next_commands(&stdout)
        .into_iter()
        .find(|c| c.contains("--apply"))
        .expect("apply line");
    assert!(line.contains(" --plan-id "), "{line}");
    let id = extract_plan_id(&line).unwrap();
    assert!(
        !stdout.contains("needs no plan-id"),
        "a Destructive-tier line carries an id: {stdout}"
    );

    // One --only removed: the same id, a different selection: refused as changed, nothing deleted.
    let shorter = plain(
        &f,
        &[
            "--apply",
            "--kinds",
            "fixture-destructive",
            "--root",
            r,
            "--only",
            a.to_str().unwrap(),
            "--plan-id",
            &id,
        ],
    );
    let err = text_of(&shorter).1;
    assert_eq!(shorter.status.code(), Some(64), "{err}");
    assert!(err.contains("changed") || err.contains("differs"), "{err}");
    assert!(a.is_dir() && b.is_dir());

    // The saved entry keeps the flags with --only.
    let saved = std::fs::read_to_string(f.xdg("state").join("dunnage/last_plan")).unwrap();
    assert!(saved.contains("--only"), "{saved}");

    // The whole pasted line is accepted and removes exactly the two marked directories.
    let ok = run_line(&f, &line);
    assert_eq!(ok.status.code(), Some(0), "{}", text_of(&ok).1);
    assert!(!a.exists() && !b.exists());
    assert!(root.join("stale-c").is_dir(), "the unmarked sibling stays");
}

#[test]
fn a_safe_tier_next_line_should_state_that_only_is_a_filter_without_a_plan_id() {
    let f = Fixture::new();
    let cache = f.home().join(".npm/_cacache");
    std::fs::create_dir_all(cache.join("sub")).unwrap();
    std::fs::write(cache.join("sub/blob"), vec![3u8; 20_000]).unwrap();
    let out = plain(
        &f,
        &["--kinds", "caches", "--only", cache.to_str().unwrap()],
    );
    let (stdout, stderr) = text_of(&out);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    let at = lines
        .iter()
        .position(|l| {
            l.starts_with("  dunnage --apply") && l.contains("--only") && !l.contains("--plan-id")
        })
        .unwrap_or_else(|| panic!("no id-less apply line with --only:\n{stdout}"));
    assert!(
        lines[at].ends_with(&format!("--only {}", cache.display())),
        "{}",
        lines[at]
    );
    assert!(
        lines[at + 1].starts_with("      "),
        "the explanation follows"
    );
    let note = lines[at + 2..]
        .iter()
        .take_while(|l| l.starts_with("      "))
        .map(|l| l.trim())
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(note, dunnage_core::plan::NO_PLAN_ID_ONLY_NOTE, "{stdout}");
    // Without --only there is no such line.
    let plain_out = plain(&f, &["--kinds", "caches"]);
    assert!(!text_of(&plain_out).0.contains("needs no plan-id; --only"));
}

#[test]
fn help_should_document_only_as_repeatable_absolute_canonical_and_bounded() {
    let f = Fixture::new();
    let out = plain(&f, &["--help"]);
    let help = text_of(&out).0;
    assert!(
        help.contains("repeatable, absolute, canonical; each must be an item of the selected kinds; at most 20, 512 bytes each"),
        "{help}"
    );
}

#[test]
fn hostile_names_should_arrive_as_one_argument_through_the_printed_only_flag() {
    let f = Fixture::new();
    let root = f.home().join("code/proj");
    let hostile = ["stale-a b", "stale-$(touch INJECTED)", "stale-it's"];
    for n in hostile {
        aged_dir(&root.join(n), 9_000, 90);
    }
    let mut args = vec![
        "--kinds".to_string(),
        "fixture-destructive".into(),
        "--root".into(),
        root.display().to_string(),
    ];
    for n in hostile {
        args.push("--only".into());
        args.push(root.join(n).display().to_string());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = plain(&f, &refs);
    let (stdout, stderr) = text_of(&out);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    let line = stdout
        .lines()
        .find(|l| l.starts_with("  dunnage --apply"))
        .expect("apply line")
        .trim()
        .to_string();
    // The line parses back, under sh, to exactly the three paths.
    let probe = f.command_for("sh")
        .arg("-c")
        .arg(format!("dunnage() {{ for a in \"$@\"; do printf '%s\\n' \"$a\"; done; }}; set -- {}; shift; dunnage \"$@\"", line.replacen("dunnage", "x", 1)))
        .output()
        .unwrap();
    let got = text_of(&probe).0;
    for n in hostile {
        assert!(
            got.lines().any(|l| Path::new(l) == root.join(n)),
            "{n} in {got}"
        );
    }
    assert!(!root.join("INJECTED").exists() && !Path::new("INJECTED").exists());
}

#[cfg(feature = "gate-c")]
mod build_dirs {
    use super::*;

    fn project(f: &Fixture, rel: &str, lock: &str) -> PathBuf {
        let dir = f.home().join("code").join(rel);
        let nm = dir.join("node_modules");
        aged_dir(&nm, 20_000, 90);
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        std::fs::write(dir.join("package-lock.json"), lock).unwrap();
        nm
    }

    #[test]
    fn only_should_select_one_of_three_stale_build_dirs_and_bind_into_the_gate() {
        let f = Fixture::new();
        let nms = [
            project(&f, "alpha", "la"),
            project(&f, "beta", "lb"),
            project(&f, "gamma", "lg"),
        ];
        let all = json(&plain(&f, &["--kinds", "build-dirs", "--json"]));
        let one = json(&plain(
            &f,
            &[
                "--kinds",
                "build-dirs",
                "--only",
                nms[1].to_str().unwrap(),
                "--json",
            ],
        ));
        assert_eq!(paths(&one), vec![nms[1].to_str().unwrap().to_string()]);
        assert_ne!(one["plan_id"], all["plan_id"]);
        let id = one["plan_id"].as_str().unwrap();

        // Dropping the only value is a different selection under the same id.
        let wide = plain(&f, &["--apply", "--kinds", "build-dirs", "--plan-id", id]);
        assert_eq!(wide.status.code(), Some(64), "{}", text_of(&wide).1);
        assert!(nms.iter().all(|n| n.is_dir()));

        // The pasted line is accepted and removes only that one.
        let ok = plain(
            &f,
            &[
                "--apply",
                "--kinds",
                "build-dirs",
                "--only",
                nms[1].to_str().unwrap(),
                "--plan-id",
                id,
            ],
        );
        assert_eq!(ok.status.code(), Some(0), "{}", text_of(&ok).1);
        assert!(nms[0].is_dir() && !nms[1].exists() && nms[2].is_dir());
    }
}

#![cfg(feature = "gate-c")]
#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! Story 5.1.3: safety guarantees that need a spawned binary, a mount namespace or the repository's checker scripts.
//! Compiled with `--features disk-cleanup/gate-c` or later (`make safety` passes gate-d). Fixtures only.

mod common;

use common::{aged_dir, repo_root, unshare_works, Fixture};
use std::process::Command;

/// A bind mount below the build dir is a different mount (same `st_dev`, different mount id): the engine must not
/// descend into or remove it. Needs `unshare -rm`; a named skip without it, a failure under `DC_REQUIRE_BIND_MOUNT=1`.
#[test]
fn safety_bind_mount_below_target_refused() {
    if !unshare_works() {
        assert!(
            std::env::var_os("DC_REQUIRE_BIND_MOUNT").is_none(),
            "DC_REQUIRE_BIND_MOUNT is set and unshare -rm does not work here"
        );
        eprintln!("SKIP: safety_bind_mount_below_target_refused: user namespaces are disabled (covered by FaultDirOps report_mnt_id)");
        return;
    }
    let f = Fixture::new();
    let src = f.root().join("bind-source");
    aged_dir(&src, 4096, 90);
    let app = f.home().join("code").join("app");
    let nm = app.join("node_modules");
    aged_dir(&nm.join("mounted"), 128, 90);
    aged_dir(&nm, 20_000, 90);
    std::fs::write(app.join("package.json"), "{}").unwrap();
    std::fs::write(app.join("package-lock.json"), "lock").unwrap();
    let out_dir = f.root().join("out");
    std::fs::create_dir_all(&out_dir).unwrap();

    let script = r#"
        set -e
        mount --bind "$SRC" "$MNT"
        "$BIN" --kinds build-dirs --json > "$OUT/plan.json" 2> "$OUT/plan.err"
        id=$(sed -n 's/.*"plan_id": *"\([0-9a-f]\{8\}\)".*/\1/p' "$OUT/plan.json" | head -1)
        test -n "$id"
        set +e
        "$BIN" --apply --kinds build-dirs --plan-id "$id" > "$OUT/apply.out" 2> "$OUT/apply.err"
        echo $? > "$OUT/rc"
        # still mounted and still the source's content, seen from inside the namespace
        test -f "$MNT/f" && echo mounted-intact > "$OUT/inside"
    "#;
    let out = f
        .command_for("unshare")
        .env("SRC", &src)
        .env("MNT", nm.join("mounted"))
        .env("OUT", &out_dir)
        .env("BIN", env!("CARGO_BIN_EXE_disk-cleanup"))
        .args(["-rm", "sh", "-c", script])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let read = |n: &str| std::fs::read_to_string(out_dir.join(n)).unwrap_or_default();
    let ctx = format!(
        "unshare: {} {}\nplan.err: {}\napply.out: {}\napply.err: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
        read("plan.err"),
        read("apply.out"),
        read("apply.err")
    );
    eprintln!("{ctx}\nrc: {}", read("rc").trim());
    assert!(out_dir.join("rc").exists(), "the apply never ran\n{ctx}");
    assert_eq!(
        std::fs::read(src.join("f")).unwrap_or_default().len(),
        4096,
        "the bind mount's source lost its file\n{ctx}"
    );
    assert!(
        read("apply.out").contains("cross-device"),
        "the guard did not refuse the bind mount as cross-device\n{ctx}"
    );
    assert_eq!(read("inside").trim(), "mounted-intact", "{ctx}");
    assert!(
        nm.join("mounted").is_dir(),
        "the mount point itself was removed\n{ctx}"
    );
}

fn run_script(args: &[&str]) -> (bool, String) {
    let out = Command::new(repo_root().join("scripts").join(args[0]))
        .args(&args[1..])
        .current_dir(repo_root())
        .output()
        .unwrap();
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// The token-site checker and the release-feature checker pass their own positive and negative controls
/// (Story 0.1.3): a checker that cannot fail would make "only gated code mints a token" unverifiable.
#[test]
fn safety_token_site_and_release_feature_checks_selftest() {
    for args in [
        &["check-token-sites.sh", "--selftest"][..],
        &["check-release-features.sh", "--selftest"][..],
    ] {
        let (ok, text) = run_script(args);
        assert!(ok, "{args:?} failed:\n{text}");
    }
}

/// The worktree-removal grep (argv array, split lines, builder, format string) catches its fixtures and the
/// repository itself is clean (Story 2.1.4).
#[test]
fn safety_worktree_removal_grep_fixtures_and_repo_clean() {
    let (ok, text) = run_script(&["check-no-stray-unlink.sh", "--selftest"]);
    assert!(ok, "selftest failed:\n{text}");
    let (ok, text) = run_script(&["check-no-stray-unlink.sh"]);
    assert!(
        ok,
        "the repository has a stray unlink or worktree removal:\n{text}"
    );
}

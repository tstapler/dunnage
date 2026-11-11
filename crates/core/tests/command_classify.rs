#![allow(clippy::unwrap_used, clippy::expect_used, non_snake_case)]
//! The verified test list of the command allowlist (prototype `implementation/prototypes/command-allowlist/`).

use disk_cleanup_core::ports::{allowlist_row_counts, classify, Class, Cmd};

fn allowed_mutating(c: &Cmd) -> bool {
    classify(c) == Some(Class::Mutating)
}

fn allowed_read_only(c: &Cmd) -> bool {
    classify(c) == Some(Class::ReadOnly)
}

fn c(line: &str) -> Cmd {
    let mut it = line.split_whitespace();
    Cmd {
        program: it.next().unwrap().into(),
        args: it.map(Into::into).collect(),
        mutating: false,
    }
}

#[test]
fn classify_should_accept_every_mutating_command_the_plan_runs_When_each_of_15_argvs_given() {
    for l in [
        "docker image rm sha256:ab12",
        "docker volume rm myvol",
        "docker builder prune --force --filter until=24h",
        "btrfs balance start -dusage=0 /",
        "btrfs balance start -dusage=5 /",
        "btrfs balance start -dusage=10 /",
        "btrfs balance start -dusage=20 /",
        "btrfs balance start -dusage=40 /",
        "fstrim -v /",
        "go clean -cache",
        "pip cache purge",
        "pip3 cache purge",
        "brew cleanup",
        "xcrun simctl delete unavailable",
        "apt-get clean",
    ] {
        assert!(allowed_mutating(&c(l)), "should allow: {l}");
        assert!(!allowed_read_only(&c(l)), "{l} is not read-only");
    }
}

#[test]
fn classify_should_accept_the_read_only_commands_When_each_argv_given() {
    for l in [
        "docker ps",
        "docker ps -a --filter volume=myvol",
        "docker info",
        "docker images",
        "docker volume ls",
        "docker volume inspect v",
        "docker system df",
        "docker context show",
        "btrfs balance status /",
        "go env GOCACHE",
        "pip cache dir",
        "git worktree list --porcelain",
        "git -C /r worktree list --porcelain",
        "git worktree list --porcelain -z",
        "git -C /r status --porcelain=v1 --untracked-files=normal",
        "git -C /r status --porcelain=v1 --ignored=matching --untracked-files=normal",
        "git -C /r rev-parse --git-common-dir",
        "git -C /r branch -r --contains abc123",
        "git -C /r log -1 --format=%ct",
        "lsof -F n +D /Users/u/code/app/node_modules",
    ] {
        assert!(allowed_read_only(&c(l)), "should allow read-only: {l}");
        assert!(!allowed_mutating(&c(l)), "{l} must not be mutating");
    }
}

#[test]
fn classify_should_accept_the_docker_listing_forms_with_a_template_and_refuse_malformed_ones() {
    // templates contain spaces, so these are built from explicit argv vectors
    let mk = |a: &[&str]| Cmd {
        program: "docker".into(),
        args: a.iter().map(|x| (*x).into()).collect(),
        mutating: false,
    };
    for a in [
        &["images", "--format", "{{json .}}"][..],
        &[
            "images",
            "--filter",
            "dangling=true",
            "--format",
            "{{json .}}",
        ],
        &["volume", "ls", "--format", "{{json .}}"],
        &["system", "df", "-v", "--format", "json"],
        &["version", "--format", "{{.Server.Version}}"],
        &["ps", "-a", "--format", "{{.Image}}"],
        &[
            "ps",
            "-a",
            "--filter",
            "volume=myvol",
            "--format",
            "{{.ID}}",
        ],
    ] {
        assert!(allowed_read_only(&mk(a)), "should allow read-only: {a:?}");
        assert!(!allowed_mutating(&mk(a)));
    }
    for a in [
        &["images", "--format"][..],
        &["images", "--filter", "--format", "x"],
        &["volume", "ls", "--format", "-x"],
        &["system", "df", "--format", "json", "-v"],
    ] {
        assert_eq!(classify(&mk(a)), None, "should refuse: {a:?}");
    }
}

#[test]
fn classify_should_refuse_136_argvs_whatever_the_mutating_label() {
    let refused = [
        // arbitrary deleters and interpreters
        "rm -rf /home/u/code/app/.claude/worktrees/agent-1",
        "rm -rf /",
        "find /home/u -delete",
        "rsync -a --delete /empty/ /home/u/x/",
        "python3 -c import_shutil;shutil.rmtree('/x')",
        "perl -e unlink",
        "systemd-run rm -rf /x",
        "strace rm",
        "watch rm x",
        "shred -u f",
        "unlink f",
        "mv /home/u/x /tmp/x",
        "dd if=/dev/zero of=/dev/sda",
        "truncate -s 0 f",
        // git: everything that is not an exact read-only shape
        "git clean -fdx",
        "git -C /r clean -fdx",
        "git worktree prune",
        "git -C /r worktree prune",
        "git worktree move /a /b",
        "git worktree remove /p",
        "git -C /r worktree remove /p",
        "git -C /r worktree remove --force /p",
        "git worktree remove -f -f /p",
        "git --exec-path=/x worktree remove /p",
        "git -c core.x=y worktree remove /p",
        "git -c alias.zz=worktree-remove zz /p",
        "git --config-env=alias.zz=X zz",
        "git --git-dir=/r/.git worktree remove /p",
        "git --no-pager -C /r worktree remove /p",
        "git -C /r -C /s status --porcelain=v1 --untracked-files=normal",
        "git -C worktree remove /p",
        "git -C -c status",
        "git gc --prune=now",
        "git reset --hard",
        "git checkout -- .",
        "git branch -D x",
        "git branch remove-me",
        "git branch -r --delete x",
        "git log -1 --output=/home/u/.bashrc",
        "git status",
        "git stash drop",
        "git submodule foreach rm",
        "git rm -r x",
        "git worktree list --porcelain extra",
        // dashed builtins and path-qualified programs
        "/usr/lib/git-core/git-worktree remove /p",
        "git-worktree remove /p",
        "/usr/bin/git worktree list --porcelain",
        "./git worktree list --porcelain",
        "/usr/bin/docker image rm x",
        "/bin/rm -rf x",
        // case variants (case-insensitive filesystems resolve them to the real binary)
        "Git worktree remove /p",
        "GIT worktree remove /p",
        "/usr/bin/GIT worktree remove /p",
        "Git status",
        "Docker image rm x",
        "RM -rf x",
        "Brew cleanup",
        // shells and wrappers
        "sh -c rm",
        "/bin/bash -c rm",
        "env git worktree remove /p",
        "sudo git worktree remove /p",
        "xargs git worktree remove",
        "doas rm x",
        "nohup rm x",
        "nice rm x",
        "ionice rm x",
        "timeout 5 rm x",
        "command rm x",
        "exec rm x",
        "busybox rm x",
        "time rm x",
        "setsid rm x",
        "stdbuf rm x",
        "flock /l rm x",
        "dash -c rm",
        "zsh -c rm",
        "ksh -c rm",
        "fish -c rm",
        // docker: prunes and force forms are not in the table
        "docker system prune -a",
        "docker system prune",
        "docker image prune -a",
        "docker image prune",
        "docker volume prune",
        "docker volume prune -f",
        "docker image rm",
        "docker image rm -f sha256:ab",
        "docker image rm --force x",
        "docker image rm a b",
        "docker volume rm -f x",
        "docker volume rm",
        "docker rm x",
        "docker rmi x",
        "docker run --rm -v /:/h alpine rm -rf /h",
        "docker exec c rm -rf /",
        "docker builder prune",
        "docker builder prune --all",
        "docker builder prune --force --filter",
        "docker builder prune --force --filter until=",
        "docker container prune",
        "docker network rm n",
        // btrfs, fstrim, go, pip, brew, apt-get, xcrun: only the exact shapes
        "btrfs balance start -dusage=100 /",
        "btrfs balance start /",
        "btrfs balance start -dusage=5",
        "btrfs balance start -dusage=5 -musage=5 /",
        "btrfs balance cancel /",
        "btrfs subvolume delete /s",
        "btrfs filesystem defragment -r /",
        "btrfs device remove /dev/sda /",
        "fstrim -a",
        "fstrim /",
        "fstrim -v",
        "lsof -F n +D",
        "lsof +D /x",
        "lsof -t /x",
        "lsof -F n +D -x",
        "go clean -modcache",
        "go clean",
        "go clean -cache -modcache",
        "go run x.go",
        "go env -w GOFLAGS=x",
        "pip install x",
        "pip uninstall -y x",
        "pip cache remove x",
        "brew uninstall x",
        "brew cleanup -s",
        "brew cleanup --prune=all",
        "brew install x",
        "xcrun simctl delete all",
        "xcrun simctl erase all",
        "apt-get remove x",
        "apt-get autoremove",
        "apt-get clean -y",
        "apt clean",
        // empty / odd
        "docker image rm -",
        "docker image rm --",
        "fstrim -v -a",
    ];
    assert_eq!(refused.len(), 136);
    for l in refused {
        for mutating in [true, false] {
            let mut cmd = c(l);
            cmd.mutating = mutating;
            assert_eq!(
                classify(&cmd),
                None,
                "should refuse (mutating label {mutating}): {l}"
            );
        }
    }
}

fn check_refuse_non_utf8_empty_and_nul() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let base = Cmd {
        program: "docker".into(),
        args: vec!["image".into(), "rm".into(), "x".into()],
        mutating: true,
    };
    assert!(allowed_mutating(&base));
    let mut bad = base.clone();
    bad.args[2] = OsString::from_vec(vec![0xff, 0xfe]);
    assert_eq!(classify(&bad), None);
    let mut bad = base.clone();
    bad.args[2] = OsString::from("a\0b");
    assert_eq!(classify(&bad), None);
    let mut bad = base.clone();
    bad.args[2] = OsString::new();
    assert_eq!(classify(&bad), None);
    let mut bad = base.clone();
    bad.program = OsString::from_vec(vec![b'g', 0xff]);
    assert_eq!(classify(&bad), None);
    let mut bad = base.clone();
    bad.program = OsString::new();
    assert_eq!(classify(&bad), None);
    let mut bad = base.clone();
    bad.program = "docker ".into();
    assert_eq!(classify(&bad), None);
    let mut bad = base.clone();
    bad.program = "docker\0".into();
    assert_eq!(classify(&bad), None);
}

fn check_ignore_the_label_in_both_directions() {
    let mut mislabelled = c("docker volume rm x");
    mislabelled.mutating = false;
    assert!(allowed_mutating(&mislabelled) && !allowed_read_only(&mislabelled));
    let mut labelled = c("git status");
    labelled.mutating = true;
    assert_eq!(classify(&labelled), None);
}

fn check_refuse_the_remove_worktree_argv_in_both_classes() {
    // the exact argv RealCommandRunner::remove_worktree builds; it must never be runnable through run_mutating
    let argv = c("git -C /r worktree remove /p");
    assert!(!allowed_mutating(&argv) && !allowed_read_only(&argv));
}

#[test]
fn classify_should_accept_exactly_the_15_mutating_and_27_read_only_rows() {
    assert_eq!(allowlist_row_counts(), (15, 27));
}

#[test]
fn classify_should_ignore_label_and_refuse_non_utf8_empty_nul_and_the_remove_worktree_argv() {
    check_refuse_non_utf8_empty_and_nul();
    check_ignore_the_label_in_both_directions();
    check_refuse_the_remove_worktree_argv_in_both_classes();
}

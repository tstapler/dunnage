# Releasing dunnage

Bazel builds the release archives (owner decision 2026-11-11, ADR-008), `git-cliff` writes the changelog, and
version tags are pushed by a human, not a bot. cargo-dist is gone: its config, its generated `release.yml` and the
`[package.metadata.dist]` table are deleted, and the workflow below replaces them.

## Why a human pushes the tag

release-please (and similar bots) commit and push using `GITHUB_TOKEN`, and GitHub deliberately does not let a
`GITHUB_TOKEN`-authored push trigger other `on: push` workflows (a loop-prevention measure, not a bug). A bot-pushed
tag would never start the release workflow. Pushing the tag yourself sidesteps this.

## What replaces cargo-dist

| cargo-dist (removed) | Now |
|---|---|
| `dist-workspace.toml`: targets, runners, `precise-builds` | the `targets` input of the release workflow (below) and `bazel build --config=dist //crates/cli:dunnage`, which builds the `cli` package alone |
| `[profile.dist]` in `Cargo.toml` | stays, for Cargo users and as the reference; `--config=dist` in `.bazelrc` equals it (`-c opt` plus thin LTO). `scripts/check-bazel-cargo-sync.sh` fails when the two differ |
| generated `.github/workflows/release.yml` | a hand-written, tag-triggered caller of the reusable workflow `tstapler/gha-actions/.github/workflows/rust-bazel-release.yml` (docs: `gha-actions/docs/rust-bazel-publish.md`) |
| `.github/workflows/check-release-artifacts.yml` as `local-artifacts-jobs` | the same file, now called by the caller before the release job and run on the three native runners: it builds the `--config=dist` binary with Bazel and runs `scripts/check-release-features.sh --artifact` on it. Under dist it could only check a rebuild beside the build; now the checked target, config and runner label are the release job's own (same inputs, so the same bytes: INFERRED, Bazel output is reproducible for equal inputs, not diffed across the two jobs) |
| shell installer, Homebrew installer | the reusable workflow renders and pushes the Homebrew formula (GitHub App secrets, see the gha-actions docs); the shell installer is not carried over (README install section: Homebrew and `cargo install`) |

### The caller

`.github/workflows/release.yml` is thin: `on: push: tags: ['v[0-9]+.[0-9]+.[0-9]+*']` plus a `workflow_dispatch` dry
run, `permissions: {}` at the top, a job that calls `./.github/workflows/check-release-artifacts.yml`, then a job
that `uses: tstapler/gha-actions/.github/workflows/rust-bazel-release.yml@feat/rust-bazel-publish` with
`package-name: dunnage`, `bazel-target: //crates/cli:dunnage`, `bazel-configs: dist` and `targets` left at its
default (the three below). It is not triggered yet. Pin the ref to a SHA once that branch is merged, run it as a
dry run (`workflow_dispatch`), and only then tag.

## What is configured

- Targets, each built natively on its runner (Bazel cross-compiles nothing): `x86_64-unknown-linux-gnu` on
  `ubuntu-22.04`, `aarch64-unknown-linux-gnu` on `ubuntu-24.04-arm`, `aarch64-apple-darwin` on `macos-15`. The
  x86_64 runner stays `ubuntu-22.04` (it was dist's default) so the glibc floor is at most 2.35.
- The binary: `bazel build --config=dist //crates/cli:dunnage`. `--config=dist` is `-c opt`, thin LTO and
  `--stamp` (the commit, dirty flag and scan revision `crates/cli/build.rs` bakes under Cargo come from
  `build/workspace_status.sh`). It registers every apply-capable kind (the default `--//build:gate=d`, equal to the default
  features `gate-b`, `gate-c`, `gate-d` of `crates/cli/Cargo.toml`), and `explore` is on by default. `--config=nogate` is
  the build with none.
- No `test-support` in a shipped binary: `scripts/check-release-features.sh` has a Cargo leg (`cargo tree`,
  `cargo build`) and a Bazel leg (an `aquery` over every rustc action under the target, then the marker and
  reference byte legs on the built binary). Its Bazel selftest builds a deliberately tainted binary
  (`--//build:test_support=true`) and requires the legs to fail on it.
- `cliff.toml`: Conventional Commits; `deps:` and `chore(deps):` commits are grouped under "Dependencies";
  `chore`, `docs`, `style`, `test` commits are skipped.
- crates.io stays a Cargo operation: `cargo publish --workspace` reads the Cargo manifests, which Bazel never replaced.

### Why no musl in v0.1.0

The Homebrew-on-Linux and `cargo install` paths are glibc, the owner's machine is glibc, a musl build adds a
fourth target with no hosted-runner leg to test it, and its one benefit (running on a glibc older than the build
runner's) is what the glibc floor below measures. Revisit if a user reports a glibc failure.

### glibc floor

`objdump -T <binary> | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1` on the x86_64 binary built locally on the
author's Manjaro machine printed `GLIBC_2.39`. That is the floor of a binary built against a 2.39-or-newer glibc,
not of the release, which is built on `ubuntu-22.04` (glibc 2.35, so the floor is at most 2.35: INFERRED, not
measured). Bazel's C toolchain is the runner's `gcc` (`local_config_cc`), so the floor follows the runner image.
TODO (first CI run): run the same command on the x86_64 and aarch64 archives of the first tag and write the two
figures here and in the README install section.

## One-time setup (owner)

1. Create `github.com/tstapler/dunnage` (public: the free `ubuntu-24.04-arm` and `macos-15` runners are free
   for public repositories only) and push `main`.
2. Fill the reporting address and response time in `SECURITY.md`, enable private vulnerability reporting, set the
   repository topics and social preview.
3. Homebrew (Task 5.3.4a, after the gates): create the GitHub App for `tstapler/homebrew-tap` and set the secrets and
   variable the reusable workflow names (`gha-actions/docs/rust-bazel-publish.md`), then pass
   `publish-homebrew: true` and `homebrew-app-client-id` from the caller. Until then the caller sets
   `publish-homebrew: false`, so a tag uploads a GitHub Release and nothing else.
4. crates.io (Story 5.3.3): account and token status unknown; `cargo login`, or a `CARGO_REGISTRY_TOKEN` secret.
5. The rename to dunnage (Story 5.3.0) follows `RENAME.md`.

## Cutting a release

```bash
git cliff --tag vX.Y.Z -o CHANGELOG.md   # preview/update the changelog
git add CHANGELOG.md
git commit -m "chore: release vX.Y.Z"
git tag vX.Y.Z
git push origin main --tags
```

The tag push starts the caller workflow: `check-release-artifacts.yml` on the three runners, then the reusable
workflow builds the three archives, creates the GitHub Release (notes from `CHANGELOG.md`) and, once enabled,
updates the Homebrew formula. Run the caller once as a dry run (`workflow_dispatch`) before the first tag.

## Owner waits (Gates A to D)

A gate is not reviewed, and the next wave does not start, until its precondition is attached to the gate report
(`.git/gate-reports/<gate>-<sha>.txt`, from `make gate GATE=<letter>` for the tagged commit):

- [ ] The gate report for the tagged commit is all green (0 failed).
- [ ] From Gate B on: `make safety` passes on both profiles (it first runs `scripts/check-safety-manifest.sh`, so
      a renamed or deleted `safety_` test fails here; raise `active-gate` in `ci/safety/manifest.txt` first).
- [ ] From Gate B on: `dunnage audit verify` over the live audit log reports 0 deletions outside the allowed
      roots. A violation keeps the gate closed.
- [ ] Gate B only: the full-disk rehearsal (`scripts/rehearse-full-disk.sh`, Story 2.3.3) is recorded, with the
      commands typed per case.
- [ ] Gate D only: one decision recorded in `spikes.md` for each of O-1, SM-2 and CV-1 that missed. Run
      `scripts/outcome-report.sh` (it classifies the first three real runs of 5 GiB or more from the audit log,
      never from the summary's prose); its last line names the metrics that need a decision. A FAIL run keeps
      the gate open until the finding is in `spikes.md`. Pass `--cutover YYYY-MM-DD` for O-2.
- [ ] The UX-41 and UX-42 review checklist (`design/ux.md` section 8) is ticked: UX-41 is automated for the
      catalogue texts, so the checklist covers the section 4 tables; UX-42 covers every flow of section 5.

## Owner waiver of the live reviews (2026-11-12)

On 2026-11-12 the owner decided that the shipped binary has the gated apply paths on by default (`default` of
`crates/cli/Cargo.toml` is `explore`, `gate-b`, `gate-c`, `gate-d`; Bazel's `--//build:gate` defaults to `d`) and
waived the live reviews: OWNER WAIT A to D, OWNER WAIT F and the Story 5.1.2 btrfs check were **not performed**. What
backs the apply path is the fixture-based evidence of the gate reports (`make gate GATE=D`, `make safety`,
the pty journeys on the real binary), not a live run on the owner's machine. `--apply` is live; the safety rules are
unchanged: a bare run is a dry run, `--apply` is required to delete, a destructive kind needs `--plan-id` from a dry
run of the same selection, rule F (the floor) and the allowed roots bound every delete, every delete is audit-logged
first, and the explorer's delete needs a plan and the typed count. The sections below are kept as the checklists a
reviewer can still run; read their "not registered in the shipped binary" sentences as the state before this decision.

Still in force: an unconfined `--apply` checks the gate stamp (`gate_stamp`, written by `make install-gated`), so a
binary from a release archive or Homebrew refuses a real `--apply` ("no valid gate stamp") unless the owner uses
the audited `DUNNAGE_UNGATED` break-glass. This decision registered the kinds; it did not remove that check.

## OWNER WAIT B: the first live `--apply --kinds caches`

The Gate B apply path is built but the shipped binary does not register it (`gate-b` is not a default feature of
`crates/cli`). Review it from a gated build, then make the feature a default and tag.

1. Build and gate: `make gate GATE=B`, then at once `make install-gated GATE=B` (the next gate rebuilds the release
   binary and would break the stamp). The install copies the Bazel gate binary (`.git/gate-reports/bin-<gate>`, the file the stamp names) to `~/.cargo/bin`, the stamp
   to `$XDG_STATE_HOME/dunnage/gate-stamp.json`, and runs `dunnage status`, which leaves `audit.reserve`
   and `last_plan.reserve` (1 MiB each) in the state dir; record that in the gate report.
2. Rehearse a full disk first: `scripts/rehearse-full-disk.sh --prime 'dunnage status' -- sh` and, in that
   shell, do the flow below using only the lines printed; then once more with `--no-reserve` and follow the
   `NOT usable` text. Record per case the commands typed (target 2), any step you could not finish from the
   screen, and whether the `NOT usable` text led to the safe apply. The shell's `XDG_STATE_HOME` is a 24 MiB
   tmpfs; `HOME` is your real one, so do not apply inside it until step 3 is done reading.
3. Read the dry run: `dunnage` (nothing is deleted). Read the table, the `Filesystem` block and the
   `Next:` lines. `dunnage --json | jq '.items[] | select(.status != "skipped") | [.path, .est_bytes]'` is the
   same list for scripts.
4. Apply: copy the first `Next:` line, `dunnage --apply` (caches need no plan-id). A line that carries
   `--plan-id <id>` comes from the dry run you just read; the id is spent after its apply and a changed plan
   needs a new dry run. Aim for a measured drop of at least 5 GiB (`TOO SMALL` runs are not counted).
5. Judge it: `scripts/outcome-report.sh` classifies the run from the recorded `run_end` (PASS, EXPLAINED,
   AMBER, FAIL) and prints O-1/O-2/O-3 and the reach table; it reports and never acts.
   `dunnage audit verify` must say `0 outside the allowed roots` (exit 0; 2 is a violation and keeps the
   gate closed). `make safety` must pass on both profiles.
6. On accept: add `"gate-b"` to `default` in `crates/cli/Cargo.toml`, commit, and cut `v0.2.0` from that commit.

## OWNER WAIT C: the first live `--apply --kinds build-dirs`

Build dirs (Story 3.2.4) are built but not registered in the shipped binary: `gate-c` is not a default feature of
`crates/cli`. Worktrees (Story 3.1.3) are reviewed in the same wait; its steps follow the build-dir ones.

1. Gate: `make gate GATE=C` on a clean tree, then at once `make install-gated GATE=C`; attach the report, the
   `make safety` output and `make mutants` (the guard modules in `ci/mutants.d/*.files`, survivors listed in
   `ci/mutants.d/*.survivors.txt`).
2. Read the dry run: `dunnage --kinds build-dirs`. Rows show the newest-mtime age, `kept as newest of N
   duplicates`, `not reproducible` (an `npm link`ed package or no lockfile) and `running build`. A dir younger than
   60 minutes is never offered, whatever `--min-age` says (the Notes block says when the floor was raised).
   `--keep-newest 0` offers the newest copy of every duplicate set and prints a WARNING.
3. Apply: `dunnage --apply --kinds build-dirs --plan-id <id>`, the id from the `Next:` line of that dry run
   (24 h, same user, same flags). Count how many times you hit `plan-id ... does not match the plan just computed`
   (`changed`): that number is the evidence rule of `design/ux.md` 1.4 (the id binds the item set, there is no
   tolerance in v1). A partly failed or interrupted apply spends its id; the summary says so and names the
   command. A dir that an apply emptied but could not remove has a fresh mtime and stays under the 60 minute floor
   for an hour.
4. Judge it: `dunnage audit verify` over all live runs (exit 0, `0 outside the allowed roots`), `make safety`
   on both profiles, `scripts/outcome-report.sh`.
5. Pick `--jobs` from the table in `research/spikes.md` (Story 3.2.4) if the default does not suit this disk.
6. On accept (build dirs and worktrees): add `"gate-c"` to `default` in `crates/cli/Cargo.toml`, set `active-gate C`
   in `ci/safety/kinds-by-gate.txt` and `ci/safety/manifest.txt`, commit, cut `v0.3.0` from that commit.

## OWNER WAIT D, btrfs balance (Story 4.3.2)

The balance is built but not registered in the shipped binary: `gate-d` is not a default feature of `crates/cli`.
Your root filesystem is the one it is for, and it starts almost full of chunks, so read this before the first run.
The tool never calls `sudo` and never cancels a balance.

1. Save the "before" state, as you, then as root: `dunnage status`, `sudo btrfs filesystem usage /`,
   `sudo btrfs balance status /` (must say `No balance found`), `sudo btrfs scrub status /` and
   `systemctl is-active btrfs-balance.timer` (must not say `active`; a balance that starts on its own is why the
   tool refuses to start one). Run `snapper list` too if you use snapper.
2. Read how the tool sees it: `dunnage --kinds btrfs`. As a user the row is `SKIP needs root` and the
   `Next:` line is `sudo <absolute path of the binary> --apply --kinds btrfs`. The binary prints that line and
   never runs `sudo` itself. If quotas are on, a `qgroups enabled: balance may be slow` note is printed.
3. The first live run is `-dusage=0` only. Below 1 GiB of unallocated space (`unallocated` in `status`; yours was
   1.0 MiB) the tool runs `-dusage=0` and nothing else, and prints why. Run the printed line:
   `sudo /path/to/dunnage --apply --kinds btrfs`. Nothing but `btrfs balance start -dusage=0 /` runs.
4. Read the numbers. The Notes block lists each step (`-dusage=N relocated K chunks; unallocated now X`) and ends
   with `btrfs unallocated: A before, B after`. B is read back from the filesystem after the last step, not
   computed. Check it against `sudo btrfs filesystem usage /` ("Device unallocated"); `dunnage status`
   must now show the same figure. If B did not grow, `-dusage=0` found no empty chunk, which is a real answer;
   free some data (a cache apply) so unallocated passes 1 GiB, then run the line again to let `-dusage=5`
   and the larger steps run. Every non-zero step moves at most 8 chunks (`limit=8`) and the ladder stops when a
   step moves 0 chunks, when unallocated reaches 10 GiB, or on ENOSPC (exit 2, with the state printed).
5. Ctrl-C during a step does not stop the balance: the kernel finishes it. The tool stops waiting, starts no
   further step, prints `INTERRUPTED: a btrfs balance is still running in the kernel ...` and exits 130. Check
   it with `sudo btrfs balance status /`; stop it yourself with `sudo btrfs balance cancel /`. A second Ctrl-C
   exits at once. The audit log records that step as `running` and `run_end` as `interrupted`.
6. Save the "after" state the same way as step 1 and keep both beside the gate report. `dunnage audit
   verify` and `make safety` must pass as for the other gates.

## OWNER WAIT D, btrfs trim (Story 4.3.3)

`btrfs-trim` is built but not registered in the shipped binary (`gate-d` is not a default feature). A trim hands
unused blocks back to the SSD; it frees no filesystem space and does not change `unallocated`, so the tool prints
it on its own line and never counts it as freed.

1. Dry run as you: `dunnage --kinds btrfs-trim`. The row is `SKIP needs root` and the `Next:` line is
   `sudo <absolute path of the binary> --apply --kinds btrfs-trim`. The tool never runs `sudo` itself.
2. Run that line. Exactly one command runs: `fstrim -v /` (the mount that holds your HOME). Read the output:
   `trimmed N GiB (fstrim; discarded blocks, not freed space)` with the figure `fstrim` printed. If `fstrim`
   printed text the tool could not read, the line says so instead of a number. The figure is UNVERIFIED against
   this host's `fstrim`; compare it with `sudo fstrim -v /` output by eye the first time.
3. A device without discard support ends the item as an error with `fstrim`'s own text and changes nothing.
4. Check `dunnage status` before and after: `unallocated` and `df avail` must not have moved because of the
   trim. The audit `result` line carries `trimmed_bytes` and `measured_bytes: 0`.

## OWNER WAIT F: the explorer on the real machine, and its first live delete (Story 6.5.6)

Everything below the line "Owner steps" is yours; the repository cannot do it for you. The first live explorer delete
is the owner's, as the first live `--apply` of Gates B to D was. Explorer deletion exists only in a `gate-d` build
(`explorer-selection` is registered there and nowhere else), so the steps use the gated binary. Turning it on in the
shipped binary is a separate edit of `default` in `crates/cli/Cargo.toml` and is not part of this wait.

What the repository has already proved, on fixtures, on the real binary (read the gate report, not this list):

- `crates/cli/tests/explore_pty.rs`: the terminal is restored after `q`, Ctrl-C, SIGTERM, SIGHUP, an external
  SIGINT, Ctrl-Z then `fg`, a resize and a forced panic (`DC_EXPLORE_PANIC_AT`, test-support builds only); a name
  with `ESC[2J` never reaches the screen raw; the no-delete script leaves the tree byte-identical.
- `crates/cli/tests/explore_fulldisk.rs`: state and cache on full tmpfs filesystems (`scripts/rehearse-full-disk.sh
  --cache-too`), `q` mid-scan, SIGTERM and Ctrl-C during a slow planner.
- `crates/cli/tests/explore_delete_pty.rs`: `p`, `d`, the typed count and Enter over a fixture HOME, the mount case,
  SIGTERM mid-delete, `audit verify` exit 0.
- `scripts/bench-explore.sh`: 676,296 directories, with the numbers in `research/spikes.md`.
- `scripts/check-release-features.sh`: the shipped and dist binaries contain none of the three test-support
  switches (`DC_EXPLORE_PANIC_AT`, `DC_EXPLORE_PLAN_DELAY_MS`, `DC_EXPLORE_DELETE_DELAY_MS`).

Owner steps (a real terminal, the real `$HOME`; nothing here runs unattended):

1. Gate and install: `make gate GATE=F` on a clean tree (it prints `gate F is green at <sha>` and the tag command),
   then at once `make install-gated GATE=F` (the next gate rebuilds the binary and would break the stamp). Attach
   `.git/gate-reports/F-<sha>.txt`. `make safety` must pass on both profiles.
2. Cold, then warm, with a stopwatch (or a phone video) from pressing Enter:
   - Cold: `dunnage explore --rescan ~` (ignores the saved cache and writes a new one). Note the time to the
     first frame (target 0.5 s), the time until the title says `SCAN DONE`, and whether the rows stayed put under the
     cursor while it scanned (press `j` and `k` during the scan; a row that moves away under the cursor is a fail).
     Quit with `q`.
   - Warm: `dunnage explore ~`. Note the time to the first frame and the time until the whole cached tree is
     painted (target 3 s; the title reads `cache from <age>` until it is checked, then `cache warm`). Quit with `q`.
   - Optional, machine-timed: `DC_BENCH_OWNER_MACHINE=1 scripts/bench-explore.sh` (also runs the real `$HOME`
     cold and warm; read-only) and `DC_BENCH_OWNER_MACHINE=1 scripts/bench-dry-run.sh` for the cleaner's 12.7 s.
3. `kill -9`, once, in two terminals. Terminal A: `dunnage explore ~`, wait for the first frame. Terminal B:
   `pkill -9 -f 'dunnage explore'`. Terminal A is left in the alternate screen with a hidden cursor and no echo
   (the one case no code can handle). Type `reset` and Enter (you may not see what you type) and confirm the shell is
   usable again. Record that it was.
4. Delete ONE throwaway directory below a configured root, through `p`, `d` and the typed count:
   - Make it: `d=~/code/dc-throwaway-$(date +%F); mkdir "$d" && head -c 5M /dev/urandom > "$d/blob"`. It must be
     below a root in `~/.config/dunnage/config.toml` (or a default root such as `~/code`), strictly below, and
     not your home, a mount point, a symlink or a git repository.
   - Run `dunnage explore ~/code`. Move to the directory (`j`/`k`), mark it with Space, press `p` and read the
     preview (`Scope: 1 marked item`, the size, `Guards run now`). Press `d` and read the prompt: it names the plan id
     and the item, and says `Type 1 and press Enter to delete`. Type `1` and Enter. Read the result list
     (`RESULT - 1 removed`), then Esc and `q`. The closing line reads
     `EXPLORE CLOSED - deleted 1 items, <bytes> measured. Audit log: <path>`. A different number, or `d` before `p`
     deleting anything, is a fail: record it and stop.
5. Read the audit line: `tail -n 6 ~/.local/state/dunnage/audit.jsonl | jq -c '{type, typed, count, path, status, interrupted}'`
   shows `run_start`, `explorer_confirm` (`typed` `"1"`, `count` 1), `intent`, `result` (`removed`) and `run_end`
   (`interrupted` false). Then `dunnage audit verify`: exit 0 and `0 outside the allowed roots`. A violation
   keeps the gate closed.
6. Record in the gate report: the load average at the time (`uptime`); the cold and warm first-frame and
   cached-paint times; whether rows stayed put; the `kill -9` and `reset` outcome; the closing line; the audit
   lines; the `audit verify` output; and your accept or reject. On accept, tag the gated commit yourself (the gate
   never tags): `git tag explore-gate-f <sha>` with the sha from step 1.

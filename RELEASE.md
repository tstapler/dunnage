# Releasing disk-cleanup

Follows the kibitzer/stelekit/stapler-squad pattern: `cargo-dist` builds and
publishes, `git-cliff` writes the changelog, and version tags are pushed by
a human — not a bot.

## Why a human pushes the tag

release-please (and similar bots) commit and push using `GITHUB_TOKEN`, and
GitHub deliberately does not let a `GITHUB_TOKEN`-authored push trigger other
`on: push` workflows (it's a loop-prevention measure, not a bug). That means
a bot-pushed tag would never kick off `release.yml`. Pushing the tag
yourself sidesteps this entirely.

## One-time setup

1. `cargo install cargo-dist` then `dist init` — answer its prompts, it will
   read `dist-workspace.toml` and may rewrite it against the installed
   version.
2. `dist generate` — writes `.github/workflows/release.yml`. Do not
   hand-write that workflow; regenerate it whenever `dist-workspace.toml`
   changes.
3. If publishing to a Homebrew tap: create `tstapler/homebrew-tap` on
   GitHub if it doesn't exist yet, and add a `HOMEBREW_TAP_TOKEN` repo secret
   (a PAT with push access to that tap repo).
4. If publishing to crates.io: `cargo login`, and add a `CARGO_REGISTRY_TOKEN`
   repo secret for CI.
5. If publishing to npm (e.g. wrapping the binary for `npx`): add an
   `NPM_TOKEN` repo secret.

## Cutting a release

```bash
git cliff --tag vX.Y.Z -o CHANGELOG.md   # preview/update the changelog
git add CHANGELOG.md
git commit -m "chore: release vX.Y.Z"
git tag vX.Y.Z
git push origin main --tags
```

The tag push triggers `release.yml`: cross-compiles for the configured
targets, creates the GitHub Release (using `CHANGELOG.md` for notes), and
runs whichever `publish-jobs` are configured in `dist-workspace.toml`.

## Owner waits (Gates A to D)

A gate is not reviewed, and the next wave does not start, until its precondition is attached to the gate report
(`.git/gate-reports/<gate>-<sha>.txt`, from `make gate GATE=<letter>` for the tagged commit):

- [ ] The gate report for the tagged commit is all green (0 failed).
- [ ] From Gate B on: `make safety` passes on both profiles (it first runs `scripts/check-safety-manifest.sh`, so
      a renamed or deleted `safety_` test fails here; raise `active-gate` in `ci/safety/manifest.txt` first).
- [ ] From Gate B on: `disk-cleanup audit verify` over the live audit log reports 0 deletions outside the allowed
      roots. A violation keeps the gate closed.
- [ ] Gate B only: the full-disk rehearsal (`scripts/rehearse-full-disk.sh`, Story 2.3.3) is recorded, with the
      commands typed per case.
- [ ] Gate D only: one decision recorded in `spikes.md` for each of O-1, SM-2 and CV-1 that missed. Run
      `scripts/outcome-report.sh` (it classifies the first three real runs of 5 GiB or more from the audit log,
      never from the summary's prose); its last line names the metrics that need a decision. A FAIL run keeps
      the gate open until the finding is in `spikes.md`. Pass `--cutover YYYY-MM-DD` for O-2.
- [ ] The UX-41 and UX-42 review checklist (`design/ux.md` section 8) is ticked: UX-41 is automated for the
      catalogue texts, so the checklist covers the section 4 tables; UX-42 covers every flow of section 5.

## OWNER WAIT B: the first live `--apply --kinds caches`

The Gate B apply path is built but the shipped binary does not register it (`gate-b` is not a default feature of
`crates/cli`). Review it from a gated build, then make the feature a default and tag.

1. Build and gate: `make gate GATE=B`, then at once `make install-gated GATE=B` (the next gate rebuilds the release
   binary and would break the stamp). The install copies `target/release/disk-cleanup` to `~/.cargo/bin`, the stamp
   to `$XDG_STATE_HOME/disk-cleanup/gate-stamp.json`, and runs `disk-cleanup status`, which leaves `audit.reserve`
   and `last_plan.reserve` (1 MiB each) in the state dir; record that in the gate report.
2. Rehearse a full disk first: `scripts/rehearse-full-disk.sh --prime 'disk-cleanup status' -- sh` and, in that
   shell, do the flow below using only the lines printed; then once more with `--no-reserve` and follow the
   `NOT usable` text. Record per case the commands typed (target 2), any step you could not finish from the
   screen, and whether the `NOT usable` text led to the safe apply. The shell's `XDG_STATE_HOME` is a 24 MiB
   tmpfs; `HOME` is your real one, so do not apply inside it until step 3 is done reading.
3. Read the dry run: `disk-cleanup` (nothing is deleted). Read the table, the `Filesystem` block and the
   `Next:` lines. `disk-cleanup --json | jq '.items[] | select(.status != "skipped") | [.path, .est_bytes]'` is the
   same list for scripts.
4. Apply: copy the first `Next:` line, `disk-cleanup --apply` (caches need no plan-id). A line that carries
   `--plan-id <id>` comes from the dry run you just read; the id is spent after its apply and a changed plan
   needs a new dry run. Aim for a measured drop of at least 5 GiB (`TOO SMALL` runs are not counted).
5. Judge it: `scripts/outcome-report.sh` classifies the run from the recorded `run_end` (PASS, EXPLAINED,
   AMBER, FAIL) and prints O-1/O-2/O-3 and the reach table; it reports and never acts.
   `disk-cleanup audit verify` must say `0 outside the allowed roots` (exit 0; 2 is a violation and keeps the
   gate closed). `make safety` must pass on both profiles.
6. On accept: add `"gate-b"` to `default` in `crates/cli/Cargo.toml`, commit, and cut `v0.2.0` from that commit.

## OWNER WAIT C: the first live `--apply --kinds build-dirs`

Build dirs (Story 3.2.4) are built but not registered in the shipped binary: `gate-c` is not a default feature of
`crates/cli`. Worktrees (Story 3.1.3) are reviewed in the same wait; its steps follow the build-dir ones.

1. Gate: `make gate GATE=C` on a clean tree, then at once `make install-gated GATE=C`; attach the report, the
   `make safety` output and `make mutants` (the guard modules in `ci/mutants.d/*.files`, survivors listed in
   `ci/mutants.d/*.survivors.txt`).
2. Read the dry run: `disk-cleanup --kinds build-dirs`. Rows show the newest-mtime age, `kept as newest of N
   duplicates`, `not reproducible` (an `npm link`ed package or no lockfile) and `running build`. A dir younger than
   60 minutes is never offered, whatever `--min-age` says (the Notes block says when the floor was raised).
   `--keep-newest 0` offers the newest copy of every duplicate set and prints a WARNING.
3. Apply: `disk-cleanup --apply --kinds build-dirs --plan-id <id>`, the id from the `Next:` line of that dry run
   (24 h, same user, same flags). Count how many times you hit `plan-id ... does not match the plan just computed`
   (`changed`): that number is the evidence rule of `design/ux.md` 1.4 (the id binds the item set, there is no
   tolerance in v1). A partly failed or interrupted apply spends its id; the summary says so and names the
   command. A dir that an apply emptied but could not remove has a fresh mtime and stays under the 60 minute floor
   for an hour.
4. Judge it: `disk-cleanup audit verify` over all live runs (exit 0, `0 outside the allowed roots`), `make safety`
   on both profiles, `scripts/outcome-report.sh`.
5. Pick `--jobs` from the table in `research/spikes.md` (Story 3.2.4) if the default does not suit this disk.
6. On accept (build dirs and worktrees): add `"gate-c"` to `default` in `crates/cli/Cargo.toml`, set `active-gate C`
   in `ci/safety/kinds-by-gate.txt` and `ci/safety/manifest.txt`, commit, cut `v0.3.0` from that commit.

## OWNER WAIT D, btrfs balance (Story 4.3.2)

The balance is built but not registered in the shipped binary: `gate-d` is not a default feature of `crates/cli`.
Your root filesystem is the one it is for, and it starts almost full of chunks, so read this before the first run.
The tool never calls `sudo` and never cancels a balance.

1. Save the "before" state, as you, then as root: `disk-cleanup status`, `sudo btrfs filesystem usage /`,
   `sudo btrfs balance status /` (must say `No balance found`), `sudo btrfs scrub status /` and
   `systemctl is-active btrfs-balance.timer` (must not say `active`; a balance that starts on its own is why the
   tool refuses to start one). Run `snapper list` too if you use snapper.
2. Read how the tool sees it: `disk-cleanup --kinds btrfs`. As a user the row is `SKIP needs root` and the
   `Next:` line is `sudo <absolute path of the binary> --apply --kinds btrfs`. The binary prints that line and
   never runs `sudo` itself. If quotas are on, a `qgroups enabled: balance may be slow` note is printed.
3. The first live run is `-dusage=0` only. Below 1 GiB of unallocated space (`unallocated` in `status`; yours was
   1.0 MiB) the tool runs `-dusage=0` and nothing else, and prints why. Run the printed line:
   `sudo /path/to/disk-cleanup --apply --kinds btrfs`. Nothing but `btrfs balance start -dusage=0 /` runs.
4. Read the numbers. The Notes block lists each step (`-dusage=N relocated K chunks; unallocated now X`) and ends
   with `btrfs unallocated: A before, B after`. B is read back from the filesystem after the last step, not
   computed. Check it against `sudo btrfs filesystem usage /` ("Device unallocated"); `disk-cleanup status`
   must now show the same figure. If B did not grow, `-dusage=0` found no empty chunk, which is a real answer;
   free some data (a cache apply) so unallocated passes 1 GiB, then run the line again to let `-dusage=5`
   and the larger steps run. Every non-zero step moves at most 8 chunks (`limit=8`) and the ladder stops when a
   step moves 0 chunks, when unallocated reaches 10 GiB, or on ENOSPC (exit 2, with the state printed).
5. Ctrl-C during a step does not stop the balance: the kernel finishes it. The tool stops waiting, starts no
   further step, prints `INTERRUPTED: a btrfs balance is still running in the kernel ...` and exits 130. Check
   it with `sudo btrfs balance status /`; stop it yourself with `sudo btrfs balance cancel /`. A second Ctrl-C
   exits at once. The audit log records that step as `running` and `run_end` as `interrupted`.
6. Save the "after" state the same way as step 1 and keep both beside the gate report. `disk-cleanup audit
   verify` and `make safety` must pass as for the other gates.

## OWNER WAIT D, btrfs trim (Story 4.3.3)

`btrfs-trim` is built but not registered in the shipped binary (`gate-d` is not a default feature). A trim hands
unused blocks back to the SSD; it frees no filesystem space and does not change `unallocated`, so the tool prints
it on its own line and never counts it as freed.

1. Dry run as you: `disk-cleanup --kinds btrfs-trim`. The row is `SKIP needs root` and the `Next:` line is
   `sudo <absolute path of the binary> --apply --kinds btrfs-trim`. The tool never runs `sudo` itself.
2. Run that line. Exactly one command runs: `fstrim -v /` (the mount that holds your HOME). Read the output:
   `trimmed N GiB (fstrim; discarded blocks, not freed space)` with the figure `fstrim` printed. If `fstrim`
   printed text the tool could not read, the line says so instead of a number. The figure is UNVERIFIED against
   this host's `fstrim`; compare it with `sudo fstrim -v /` output by eye the first time.
3. A device without discard support ends the item as an error with `fstrim`'s own text and changes nothing.
4. Check `disk-cleanup status` before and after: `unallocated` and `df avail` must not have moved because of the
   trim. The audit `result` line carries `trimmed_bytes` and `measured_bytes: 0`.

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

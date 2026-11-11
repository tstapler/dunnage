# Rename checklist: disk-cleanup to dunnage (Story 5.3.0)

Do it in one commit, on a quiet tree, with the owner's go-ahead. Everything below is a mechanical edit. Find what
is left with `rg -n 'disk-cleanup|disk_cleanup|DISK_CLEANUP' --glob '!target'`: the plan (Story 5.3.0) accepts only
the deliberate old-name acceptances and history.

## Where the name lives

| Place | What to change | Notes |
|---|---|---|
| `Cargo.toml` `[workspace.package] repository` | the GitHub URL | the only name in the workspace table; the five crates inherit it |
| `crates/*/Cargo.toml` `name` and path-dependency keys | `disk-cleanup` to `dunnage`; `-core`, `-adapters`, `-scan`, `-tui` to `dunnage-*` | also `[[bin]] name`, `[lib] name = "disk_cleanup_cli"` and feature names such as `disk-cleanup/gate-d` in `Makefile` |
| `dist-workspace.toml` | nothing, except `tap` once the Homebrew installer is enabled | dist reads the package and binary names from the workspace, so the name appears in no other line |
| `.github/workflows/release.yml` | nothing by hand: run `dist generate` and commit the result | generated; names the binary only through dist |
| `.github/workflows/check-release-artifacts.yml` | nothing | it calls the script below |
| `scripts/check-release-features.sh` | line `PKG=disk-cleanup CORE=disk-cleanup-core`, the `dc-flavor` marker stays | the one assignment holds the package names; the selftest strings (`disk_cleanup_core`, `disk-cleanup-core@0.1.0`) and `bin=` paths need the same edit |
| `scripts/check-*.sh`, `gate.sh`, `bench-*.sh`, `outcome-report.sh`, `rehearse-full-disk.sh`, `scale-check.sh` | binary and crate names | `rg -c` lists about 100 hits across them |
| `Makefile` | `SAFETY_FEATURES`, `install-gated` (binary, state dir, `~/.cargo/bin`) | |
| `cliff.toml`, `lefthook.yml`, `rust-toolchain.toml` | nothing | no name in them |
| `RELEASE.md`, `README.md`, `CONTRIBUTING.md`, `SECURITY.md` | prose and commands | README: also the etymology line (dropped for the working name) and the logo block of `branding/` |
| `.github/ISSUE_TEMPLATE/*`, `.github/dependabot.yml`, `CODEOWNERS` | `disk-cleanup --version` text, the repository URL in `config.yml` | |
| `ci/allowed-roots.txt`, `ci/safety/*`, `ci/mutants.d/*` | state-directory name | the audit-evidence note of Story 5.3.0 applies |
| `crates/**` sources, goldens, snapshots | `DISK_CLEANUP_*` env prefix, help text, state directory | Story 5.3.0 tasks 5.3.0a and 5.3.0b |
| `docs/json-schema.md` | binary name | |

## Order

1. `git mv` nothing: the crate directories are `crates/<role>` and keep their names.
2. Edit the manifests, then `cargo build --workspace` (regenerates `Cargo.lock`).
3. Edit scripts and the Makefile; run every `scripts/check-*.sh --selftest`.
4. `dist generate` and `dist plan`: the archive names change to `dunnage-<target>.tar.xz`.
5. Update the docs, the badges and the install lines.
6. `cargo test --workspace` and `make gate GATE=<current>` on the renamed tree.
7. Re-check the crates.io names on the day (`dunnage`, `dunnage-core`, `dunnage-adapters`, `dunnage-tui`, `dunnage-scan`).

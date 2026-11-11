# Rename checklist: disk-cleanup to dunnage (Story 5.3.0)

Do it in one commit, on a quiet tree, with the owner's go-ahead. Everything below is a mechanical edit. Find what
is left with `rg -n 'disk-cleanup|disk_cleanup|DISK_CLEANUP' --glob '!target'`: the plan (Story 5.3.0) accepts only
the deliberate old-name acceptances and history.

## Where the name lives

| Place | What to change | Notes |
|---|---|---|
| `Cargo.toml` `[workspace.package] repository` | the GitHub URL | the only name in the workspace table; the five crates inherit it |
| `crates/*/Cargo.toml` `name` and path-dependency keys | `disk-cleanup` to `dunnage`; `-core`, `-adapters`, `-scan`, `-tui` to `dunnage-*` | also `[[bin]] name`, `[lib] name = "disk_cleanup_cli"` and feature names such as `disk-cleanup/gate-d` in `Makefile` |
| Bazel: every name in one place | `MODULE.bazel` `module(name = "disk_cleanup")`; `crates/*/BUILD.bazel` `crate_name = "disk_cleanup_*"` (five libraries) and `"disk_cleanup"` (the two cli binaries); the targets `//crates/cli:disk-cleanup` and `disk-cleanup_ts`; `CARGO_BIN_EXE_disk-cleanup` in `crates/cli/BUILD.bazel`; `BAZEL_TARGET`/`CORE_TARGET` in `scripts/check-release-features.sh`; the `bazel-target` and `package-name` inputs of the release caller; the target in `.github/workflows/ci.yml` and `check-release-artifacts.yml` | Cargo `[lib] name`/`[[bin]] name` and the Bazel `crate_name`/target must stay equal: `rg -n 'disk[-_]cleanup' --glob '*.bazel' --glob '*.bzl' --glob '.bazelrc*' --glob '*.yml'` lists all of it; `scripts/check-bazel-cargo-sync.sh` and `bazel build //...` fail on a miss. The crate directories (`//crates/<role>`) and the `//build` flags (`gate`, `explore`, `test_support`) carry no product name |
| `.github/workflows/release.yml` (the thin caller, RELEASE.md) | `package-name`, `bazel-target` | the reusable workflow in tstapler/gha-actions names the archives `<package-name>-<version>-<triple>.tar.gz` |
| `.github/workflows/check-release-artifacts.yml` | the Bazel target | it calls the script below |
| `scripts/check-release-features.sh` | line `PKG=disk-cleanup CORE=disk-cleanup-core`, the `dc-flavor` marker stays | the one assignment holds the package names; the selftest strings (`disk_cleanup_core`, `disk-cleanup-core@0.1.0`) and `bin=` paths need the same edit |
| `scripts/check-*.sh`, `gate.sh`, `bench-*.sh`, `outcome-report.sh`, `rehearse-full-disk.sh`, `scale-check.sh` | binary and crate names | `rg -c` lists about 100 hits across them |
| `Makefile` | `install-gated` (binary, state dir, `~/.cargo/bin`); `scripts/gate-binary.sh` (the `//crates/cli:` target) | |
| `cliff.toml`, `lefthook.yml`, `rust-toolchain.toml` | nothing | no name in them |
| `RELEASE.md`, `README.md`, `CONTRIBUTING.md`, `SECURITY.md` | prose and commands | README: also the etymology line (dropped for the working name) and the logo block of `branding/` |
| `.github/ISSUE_TEMPLATE/*`, `.github/dependabot.yml`, `CODEOWNERS` | `disk-cleanup --version` text, the repository URL in `config.yml` | |
| `ci/allowed-roots.txt`, `ci/safety/*`, `ci/mutants.d/*` | state-directory name | the audit-evidence note of Story 5.3.0 applies |
| `crates/**` sources, goldens, snapshots | `DISK_CLEANUP_*` env prefix, help text, state directory | Story 5.3.0 tasks 5.3.0a and 5.3.0b |
| `docs/json-schema.md` | binary name | |

## Order

1. The crate directories are `crates/<role>` and keep their names; no `git mv`.
2. Edit the manifests, then `cargo build --workspace` (regenerates `Cargo.lock`).
3. Edit scripts and the Makefile; run every `scripts/check-*.sh --selftest`.
4. Bazel: rename the targets and crate names (row above), then `CARGO_BAZEL_REPIN=1 scripts/bazel.sh query @crates//:all`
   (the Cargo.lock rewrite changes the crate lock), `scripts/bazel.sh build //...` and `scripts/check-bazel-cargo-sync.sh`.
   The release archive names change to `dunnage-<version>-<target>.tar.gz` through the caller's `package-name`.
5. Update the docs, the badges and the install lines.
6. `scripts/bazel.sh test //...`, `cargo test --workspace` and `make gate GATE=<current>` on the renamed tree.
7. Re-check the crates.io names on the day (`dunnage`, `dunnage-core`, `dunnage-adapters`, `dunnage-tui`, `dunnage-scan`).

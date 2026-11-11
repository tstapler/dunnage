# AGENTS.md

Tool-agnostic project context for AI coding agents. See `CLAUDE.md` for the same expectations described via Claude Code-specific skills/slash-commands.

## Stack

- Layout: multi-crate
- CLI: `clap` derive subcommands
- MCP: `rmcp`, served over stdio via the `mcp` subcommand
- Build, test, lint: Bazel (`scripts/bazel.sh`, `MODULE.bazel`; ADR-008). Cargo manifests stay the dependency source of truth: `cargo build|test|clippy` still work, `scripts/check-bazel-cargo-sync.sh` fails when the two drift. New `tests/*.rs` files need no BUILD edit (globbed); new binaries, examples and Cargo features do
- Release: Bazel archives (`--config=dist`), triggered by a human-pushed version tag — never a bot-authored commit — with changelog via `git-cliff` (`cliff.toml`); see `RELEASE.md`
- CI: `.github/workflows/ci.yml` gates PRs; `.github/workflows/release.yml` is the thin caller of the gha-actions reusable `rust-bazel-release.yml` (RELEASE.md)

## Working Conventions

- Run `lefthook install` once after cloning.
- Keep transport code (CLI/MCP) thin; put real logic where it's independently testable (see `crates/core/`).
- Before a release, read `RELEASE.md`.

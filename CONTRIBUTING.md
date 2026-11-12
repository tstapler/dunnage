# Contributing

Issues and pull requests are welcome. A new cleaner kind is the natural first contribution: kinds sit behind the
guard traits in `crates/core`. Anything that touches the delete engine or a path guard needs the adversarial
tests extended in the same change, and those tests are written apart from the code (see `CODEOWNERS`).

## Set up

```bash
git clone https://github.com/tstapler/dunnage && cd dunnage
lefthook install                       # pre-commit: fmt, clippy, core-deps; pre-push: make gate GATE=A
cargo build --workspace
cargo test --workspace
```

Bazel (`scripts/bazel.sh`, ADR-008) keeps no cache of its own: set the disk cache, the repository cache and their size
caps once in your `~/.bazelrc` (`build --disk_cache=...`, `build --experimental_disk_cache_gc_max_size=...`) and every
workspace shares them. CI keeps its own caches through `setup-bazel`.

The toolchain is pinned in `rust-toolchain.toml`. `jq` and `ast-grep` (`cargo install ast-grep --locked`) are
needed by some `scripts/check-*.sh` checks.

## Before you open a pull request

- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` are clean.
- `make gate GATE=A` passes (it runs every `scripts/check-*.sh` that applies, fmt, clippy, tests and a release build).
- A change to a deleting code path says which `safety_` tests cover it (`make safety`).
- No new network, process or filesystem dependency in `crates/core` or `crates/tui`
  (`scripts/check-core-deps.sh`, `scripts/check-tui-deps.sh` enforce it).

## Commits

[Conventional Commits](https://www.conventionalcommits.org/): `feat:`, `fix:`, `perf:`, `refactor:`, `ci:`,
`deps:` (or `chore(deps):`) show up in the changelog (`cliff.toml`); `docs:`, `test:`, `style:` and `chore:` do not.
Mark a breaking change with `!` (`feat!:`) or a `BREAKING CHANGE:` footer.

## Releases

Maintainer only; see `RELEASE.md`. A human pushes the tag.

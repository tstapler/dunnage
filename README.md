# disk-cleanup

CLI / MCP tool.

## Install

```bash
# Homebrew (once the tap is set up — see RELEASE.md)
brew install tstapler/tap/disk-cleanup

# From source
cargo install --path .
```

## Usage

```bash
disk-cleanup                       # dry run: lists what could be freed, deletes nothing
disk-cleanup --apply               # deletes the default kinds (caches, worktrees); never prompts
disk-cleanup --kinds build-dirs    # dry run of one kind; its Next: line carries the --plan-id
disk-cleanup status                # filesystem usage and verdict (exit 0 ok, 1 warn, 2 crit)
disk-cleanup kinds                 # kinds, tiers and what this build ships
disk-cleanup audit verify          # re-check every logged deletion against ci/allowed-roots.txt
```

Dry run is the default: `-n` and `--dry-run` are accepted for old habits and exit 64 when combined with `--apply`.
There is no `--force` and no `--yes`; risky kinds must be named in `--kinds`, and destructive ones also need the
`--plan-id` of a dry run of the same selection made in the last 24 hours. Exit codes: 0 ok, 1 partial failure, 2 fatal,
3 btrfs starved, 64 usage, 74 state unreadable, 130 and 143 interrupted (`disk-cleanup --help`).

The former `metascope status [PATH] [--fs DIR] [--json]` is `disk-cleanup status [PATH] [--fs DIR] [--json]`: same
thresholds, same exit codes, and its JSON keys (`fs`, `allocation`, `unallocated_bytes`, `metadata_headroom_bytes`,
`verdict`, `findings`) keep their values, with `schema_version`, `mode`, `exit_code`, `filesystem` and `errors` added.

## Development

```bash
lefthook install   # activate pre-commit/pre-push hooks
cargo test --workspace
```

See `RELEASE.md` for how versions are cut and published.

## License

MIT — see `LICENSE`.

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
disk-cleanup run
disk-cleanup mcp   # serve as an MCP server over stdio
```

## Development

```bash
lefthook install   # activate pre-commit/pre-push hooks
cargo test --workspace
```

See `RELEASE.md` for how versions are cut and published.

## License

MIT — see `LICENSE`.

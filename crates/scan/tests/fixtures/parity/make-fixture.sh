#!/usr/bin/env bash
# make-fixture.sh <empty-or-absent-dir> : builds the scan parity fixture (Story 0.1.5, ADR-007).
# Every regular file is 0 bytes or 8192 bytes so allocation (inline vs extent) is the same on btrfs, ext4 and tmpfs,
# and every mtime is set with `touch -d` so the tree is reproducible. Absolute paths are used only inside the
# worktree `.git` pointer files, which the scanner reads to attribute worktrees to their main repository.
set -euo pipefail
[ $# -eq 1 ] || { echo "usage: $0 <dir>" >&2; exit 64; }
mkdir -p "$1"; root=$(cd "$1" && pwd -P)
[ -z "$(ls -A "$root")" ] || { echo "refusing: $root is not empty" >&2; exit 1; }

blob() { head -c 8192 /dev/zero > "$1"; }
repo() {  # repo <dir>: a git-like main checkout
  mkdir -p "$1/.git/objects" "$1/.git/worktrees" "$1/src"
  echo 'ref: refs/heads/main' > "$1/.git/HEAD"; blob "$1/.git/objects/pack1"
  : > "$1/src/main.rs"; blob "$1/src/lib.rs"
}
node_modules() {  # node_modules <dir> <lockfile-text>: package.json next to node_modules, one lockfile
  echo '{}' > "$1/package.json"; printf '%s\n' "$2" > "$1/package-lock.json"
  mkdir -p "$1/node_modules/a" "$1/node_modules/b/lib"
  : > "$1/node_modules/a/index.js"; blob "$1/node_modules/b/lib/x.js"; : > "$1/node_modules/b/package.json"
}
cargo_target() {  # cargo_target <dir> <lockfile-text>
  printf '[package]\nname="x"\n' > "$1/Cargo.toml"; printf '%s\n' "$2" > "$1/Cargo.lock"
  mkdir -p "$1/target/debug/deps"; blob "$1/target/debug/deps/libx.rlib"; : > "$1/target/debug/x"
}

# repoA: main checkout with node_modules and a cargo target, plus a linked worktree with the same lockfiles
repo "$root/repoA"; node_modules "$root/repoA" 'lock-one'; cargo_target "$root/repoA" 'cargo-one'
mkdir -p "$root/repoA/.git/worktrees/wt1" "$root/repoA-wt1"
# padded to 256 bytes (the reader trims it) so the file's size does not depend on how long $root is
{ printf 'gitdir: %s/repoA/.git/worktrees/wt1' "$root"; } | head -c 255 | awk '{ printf "%-255s\n", $0 }' > "$root/repoA-wt1/.git"
mkdir -p "$root/repoA-wt1/src"; : > "$root/repoA-wt1/src/main.rs"
node_modules "$root/repoA-wt1" 'lock-one'      # same lockfile hash as repoA: one duplicate cluster
cargo_target "$root/repoA-wt1" 'cargo-one'     # second cluster, two build dirs with one lockfile hash

# repoB: a second family with its own, different lockfile, a hardlink and a symlink
repo "$root/repoB"; node_modules "$root/repoB" 'lock-two'
ln "$root/repoB/src/lib.rs" "$root/repoB/src/lib-link.rs"
ln -s lib.rs "$root/repoB/src/lib-sym.rs"
# a directory the scanner cannot read (mode 000) and a loose tree that is not a repository
mkdir -p "$root/repoB/private/inner"; : > "$root/repoB/private/inner/f"
mkdir -p "$root/loose/docs"; blob "$root/loose/docs/notes.txt"; : > "$root/loose/empty"

# deterministic mtimes, deepest paths first so a parent is never touched after its child
find "$root" -mindepth 1 ! -type l -printf '%d %p\n' | sort -rn | cut -d' ' -f2- |
  while IFS= read -r p; do touch -d '2024-01-02 03:04:05 UTC' "$p"; done
find "$root" -type l -exec touch -h -d '2024-01-02 03:04:05 UTC' {} +
touch -d '2024-01-02 03:04:05 UTC' "$root"
chmod 000 "$root/repoB/private"   # last, so nothing above needs to read it; callers restore it with chmod 755 before rm -r

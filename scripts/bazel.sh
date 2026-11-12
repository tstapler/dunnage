#!/usr/bin/env bash
# Local entry point for Bazel: bazelisk with a bounded job count and a TMPDIR outside $HOME.
#   scripts/bazel.sh test //... --config=gate-d
# The output base, the disk cache, the repository cache and their size caps come from the user's ~/.bazelrc, shared by
# every workspace on the machine; this script sets none of them.
# Environment (all optional):
#   DC_BAZEL_PLAIN=1      pass arguments through unchanged (CI: setup-bazel owns the caches and output base)
#   DC_BAZEL_JOBS         --jobs and cpu count   (default: half the cores, at most 8; the machine is often shared)
# Why TMPDIR: crate_universe splices Cargo workspaces under $TMPDIR and refuses to run when a ~/.cargo/config.toml is
# found in a parent directory, which is the case for any TMPDIR below $HOME.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
# Tests tagged requires-repo-checkout run the repo-level scripts on the real tree, not on Bazel's symlink forest.
export DC_REPO_ROOT=${DC_REPO_ROOT:-$root}
case "${TMPDIR:-}" in "" | "$HOME"/*) export TMPDIR=/tmp ;; esac
cores=$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)
jobs=${DC_BAZEL_JOBS:-$(( cores / 2 > 8 ? 8 : (cores / 2 < 2 ? 2 : cores / 2) ))}
bz=${DC_BAZELISK:-$(command -v bazelisk || command -v bazel || true)}
[ -n "$bz" ] || { echo "scripts/bazel.sh: neither bazelisk nor bazel is on PATH (brew install bazelisk)" >&2; exit 127; }
if [ "${DC_BAZEL_PLAIN:-0}" = 1 ]; then exec "$bz" "$@"; fi
if [ $# -eq 0 ]; then exec "$bz"; fi
cmd=$1; shift
case $cmd in
  build | test | run | cquery | aquery | coverage)
    exec "$bz" "$cmd" --jobs="$jobs" --local_resources=cpu="$jobs" "$@" ;;
  *) exec "$bz" "$cmd" "$@" ;;
esac

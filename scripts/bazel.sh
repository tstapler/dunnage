#!/usr/bin/env bash
# Local entry point for Bazel: bazelisk with a deliberate output base and disk cache, and a TMPDIR outside $HOME.
#   scripts/bazel.sh test //... --config=gate-d
# Environment (all optional):
#   DC_BAZEL_OUTPUT_ROOT  output user root       (default ~/.cache/dc-bazel; ~15-25 GB once everything is built)
#   DC_BAZEL_DISK_CACHE   disk cache directory   (default ~/.cache/dc-bazel-disk-cache; ~3-6 GB)
#   DC_BAZEL_JOBS         --jobs and cpu count   (default: half the cores, at most 8; the machine is often shared)
# Why TMPDIR: crate_universe splices Cargo workspaces under $TMPDIR and refuses to run when a ~/.cargo/config.toml is
# found in a parent directory, which is the case for any TMPDIR below $HOME.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
case "${TMPDIR:-}" in "" | "$HOME"/*) export TMPDIR=/tmp ;; esac
out=${DC_BAZEL_OUTPUT_ROOT:-$HOME/.cache/dc-bazel}
cache=${DC_BAZEL_DISK_CACHE:-$HOME/.cache/dc-bazel-disk-cache}
cores=$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)
jobs=${DC_BAZEL_JOBS:-$(( cores / 2 > 8 ? 8 : (cores / 2 < 2 ? 2 : cores / 2) ))}
bz=${DC_BAZELISK:-$(command -v bazelisk || command -v bazel || true)}
[ -n "$bz" ] || { echo "scripts/bazel.sh: neither bazelisk nor bazel is on PATH (brew install bazelisk)" >&2; exit 127; }
if [ $# -eq 0 ]; then exec "$bz" --output_user_root="$out"; fi
cmd=$1; shift
case $cmd in
  build | test | run | cquery | aquery | coverage)
    exec "$bz" --output_user_root="$out" "$cmd" --disk_cache="$cache" --jobs="$jobs" --local_resources=cpu="$jobs" "$@" ;;
  *) exec "$bz" --output_user_root="$out" "$cmd" "$@" ;;
esac

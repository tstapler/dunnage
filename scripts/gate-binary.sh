#!/usr/bin/env bash
# gate-binary.sh <A-F|none|gate-b|gate-c|gate-d>   builds the release binary of that gate with Bazel and prints its path.
# "Release" is `-c opt` (== cargo --release) with the gate's registry (`--config=gate-X`, which also stamps the commit).
# The shipped archives use --config=dist on top (thin LTO); the gate measures the same code without the LTO wait.
# Bazel reuses one output path (bazel-out/k8-opt/bin/...) for every gate, so a caller that keeps the binary must copy it.
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
bz=${DC_BAZEL:-$root/scripts/bazel.sh}
cfg=()
case "${1:-}" in
  A | none | "") ;;
  B | b | gate-b) cfg=(--config=gate-b) ;;
  C | c | gate-c) cfg=(--config=gate-c) ;;
  D | E | F | d | gate-d) cfg=(--config=gate-d) ;;
  *) echo "usage: $0 <A-F|none|gate-b|gate-c|gate-d>" >&2; exit 64 ;;
esac
target=//crates/cli:dunnage
"$bz" build -c opt "${cfg[@]}" "$target" >&2
rel=$("$bz" cquery -c opt "${cfg[@]}" --output=files "$target" 2>/dev/null | head -1)
exec_root=$("$bz" info -c opt "${cfg[@]}" execution_root 2>/dev/null)
[ -n "$rel" ] && [ -f "$exec_root/$rel" ] || { echo "gate-binary.sh: no binary for $target" >&2; exit 1; }
printf '%s\n' "$exec_root/$rel"

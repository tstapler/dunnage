#!/usr/bin/env bash
# capture-old-scan.sh <old-metascope-binary> : one-off oracle capture for Story 0.1.5 (ADR-007), run BEFORE the
# import with the binary built from the metascope checkout at 3d36581. Writes the parity artefacts beside the
# fixture generator. The old binary only reads the fixture; the metascope checkout is never touched.
set -euo pipefail
bin=${1:?usage: capture-old-scan.sh <old-metascope-binary>}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
out=$here/crates/scan/tests/fixtures/parity
base=${DC_TEST_ROOT:-$HOME/.cache/dc-fixtures}
mkdir -p "$base"; work=$(mktemp -d -p "$base" parity-capture.XXXXXX)
trap 'chmod -R u+rwx "$work" 2>/dev/null; rm -rf "$work"' EXIT
"$out/make-fixture.sh" "$work/fx"
root=$(cd "$work/fx" && pwd -P)
# --threads 1: with several threads the owner of a multiply-linked inode is whichever thread saw it first
args=(scan --backend walker --no-cache --threads 1)
"$bin" "${args[@]}" --json "$root" > "$out/expected.json"
"$bin" "${args[@]}" "$root" > "$out/expected.txt"
printf '%s\n' "$root" > "$out/expected.root"
"$out/fixture-manifest.sh" "$root" > "$out/fixture.manifest"
echo "captured $(wc -l < "$out/expected.json") json lines, root $root"

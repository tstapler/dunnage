#!/usr/bin/env bash
# check-scan-deps.sh [repo-root] | --selftest
# disk-cleanup-scan is a leaf (ADR-007 decision 1): its direct dependencies are exactly serde, thiserror, rustix and
# libc (serde_json is a dev-dependency), so it cannot name core, adapters, tui or ApplyToken.
set -u -o pipefail
want_deps='libc rustix serde thiserror'

check() {
  local root=${1:-.} normal dev
  command -v jq >/dev/null || { echo "check-scan-deps: jq is required" >&2; return 2; }
  local meta
  meta=$(cd "$root" && cargo metadata --no-deps --offline --format-version 1) || { echo "check-scan-deps: cargo metadata failed" >&2; return 2; }
  jq -e '.packages[] | select(.name == "disk-cleanup-scan")' <<<"$meta" >/dev/null || { echo "check-scan-deps: package disk-cleanup-scan not found" >&2; return 2; }
  normal=$(jq -r '.packages[] | select(.name == "disk-cleanup-scan") | .dependencies[] | select(.kind == null) | .name' <<<"$meta" | sort | tr '\n' ' ' | sed 's/ $//')
  dev=$(jq -r '.packages[] | select(.name == "disk-cleanup-scan") | .dependencies[] | select(.kind == "dev") | .name' <<<"$meta" | sort | tr '\n' ' ' | sed 's/ $//')
  if [ "$normal" != "$want_deps" ]; then echo "FAIL: disk-cleanup-scan direct dependencies are [$normal], must be exactly [$want_deps]"; return 1; fi
  if [ "$dev" != "serde_json" ]; then echo "FAIL: disk-cleanup-scan dev-dependencies are [$dev], must be exactly [serde_json]"; return 1; fi
  echo "ok: disk-cleanup-scan depends on exactly $want_deps (dev: serde_json)"
}

selftest() {
  d=""
  local self bad=0 n=0 out rc
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "${d:-}"' EXIT INT TERM
  expect() { local want=$1 name=$2 pat=$3; shift 3; out=$("$@" 2>&1); rc=$?
    if [ "$rc" = "$want" ] && grep -q -- "$pat" <<<"$out"; then n=$((n+1)); echo "ok: $name"; else echo "SELFTEST BAD: $name (rc=$rc): $out"; bad=$((bad+1)); fi; }
  mkdir -p "$d/r/crates/scan/src" "$d/r/crates/core/src"
  printf '[workspace]\nresolver = "2"\nmembers = ["crates/scan", "crates/core"]\n' > "$d/r/Cargo.toml"
  printf '[package]\nname = "disk-cleanup-core"\nversion = "0.1.0"\nedition = "2021"\n' > "$d/r/crates/core/Cargo.toml"
  : > "$d/r/crates/core/src/lib.rs"; : > "$d/r/crates/scan/src/lib.rs"
  manifest='[package]\nname = "disk-cleanup-scan"\nversion = "0.1.0"\nedition = "2021"\n[dependencies]\nserde = "1"\nthiserror = "2"\nrustix = "1"\nlibc = "0.2"\n[dev-dependencies]\nserde_json = "1"\n'
  printf "$manifest" > "$d/r/crates/scan/Cargo.toml"
  expect 0 "the exact allowed dependency set passes" "^ok:" "$self" "$d/r"
  printf "$manifest" > "$d/r/crates/scan/Cargo.toml"
  printf '\n[dependencies.disk-cleanup-core]\npath = "../core"\n' >> "$d/r/crates/scan/Cargo.toml"
  expect 1 "core added to the scan manifest fails" "disk-cleanup-core" "$self" "$d/r"
  printf "$manifest" | grep -v '^libc' > "$d/r/crates/scan/Cargo.toml"
  expect 1 "a missing allowed dependency also fails (set must be exact)" "must be exactly" "$self" "$d/r"
  printf "$manifest" | sed 's/^serde_json = "1"/serde_json = "1"\ntempfile = "3"/' > "$d/r/crates/scan/Cargo.toml"
  expect 1 "an extra dev-dependency fails" "dev-dependencies" "$self" "$d/r"
  sed -i 's/disk-cleanup-scan/other/' "$d/r/crates/scan/Cargo.toml"
  expect 2 "a missing disk-cleanup-scan package fails closed" "not found" "$self" "$d/r"
  echo "selftest: $n ok, $bad bad"; [ $bad = 0 ]
}

if [ "${1:-}" = --selftest ]; then selftest; else check "${1:-.}"; fi

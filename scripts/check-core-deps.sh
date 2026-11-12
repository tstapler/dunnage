#!/usr/bin/env bash
# check-core-deps.sh [repo-root] | --selftest
# dunnage-core must not depend directly on rustix or libc, nor use them or raw deletion calls in source.
# Direct dependencies only: dunnage-scan (a read-only library) lists both transitively and that is accepted.
set -u -o pipefail
forbidden_deps='rustix libc'
forbidden_src='rustix::|libc::|std::fs::remove|unlinkat'

check() {
  local root=${1:-.} bad=0 deps dep hits
  command -v jq >/dev/null || { echo "check-core-deps: jq is required" >&2; return 2; }
  deps=$(cd "$root" && cargo metadata --no-deps --offline --format-version 1 \
    | jq -r '.packages[] | select(.name == "dunnage-core") | .dependencies[].name') || { echo "check-core-deps: cargo metadata failed" >&2; return 2; }
  for dep in $forbidden_deps; do
    if grep -qx "$dep" <<<"$deps"; then echo "FAIL: dunnage-core depends directly on $dep"; bad=1; fi
  done
  if [ -d "$root/crates/core" ]; then
    # Comment lines are stripped first so docs in core may name unlinkat.
    # shellcheck disable=SC2016
    hits=$(cd "$root" && find crates/core -type f -name '*.rs' -print0 \
      | xargs -0 -r awk -v pat="$forbidden_src" '
          { line = $0; sub(/^[ \t]*\/\/.*/, "", line); if (line ~ pat) printf "%s:%d: %s\n", FILENAME, FNR, $0 }')
    if [ -n "$hits" ]; then echo "FAIL: forbidden use in core source:"; echo "$hits"; bad=1; fi
  fi
  [ $bad = 0 ] && echo "ok: dunnage-core has no direct rustix/libc dependency or source use"
  return $bad
}

selftest() {
  d=""
  local self bad=0 n=0 out rc
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "${d:-}"' EXIT INT TERM
  expect() { local want=$1 name=$2 pat=$3; shift 3; out=$("$@" 2>&1); rc=$?
    if [ "$rc" = "$want" ] && grep -q -- "$pat" <<<"$out"; then n=$((n+1)); echo "ok: $name"; else echo "SELFTEST BAD: $name (rc=$rc): $out"; bad=$((bad+1)); fi; }
  mkdir -p "$d/r/crates/core/src"
  printf '[workspace]\nresolver = "2"\nmembers = ["crates/core"]\n' > "$d/r/Cargo.toml"
  printf '[package]\nname = "dunnage-core"\nversion = "0.1.0"\nedition = "2021"\n' > "$d/r/crates/core/Cargo.toml"
  printf '//! mentions unlinkat in a doc comment only\npub fn f() {}\n' > "$d/r/crates/core/src/lib.rs"
  expect 0 "clean fixture passes (doc comment naming unlinkat is ignored)" "^ok:" "$self" "$d/r"
  printf '\n[dependencies]\nlibc = "0.2"\n' >> "$d/r/crates/core/Cargo.toml"
  expect 1 "a libc dependency fails naming it" "depends directly on libc" "$self" "$d/r"
  sed -i '/^\[dependencies\]/,$d' "$d/r/crates/core/Cargo.toml"
  printf '\n[dependencies]\nrustix = "1"\n' >> "$d/r/crates/core/Cargo.toml"
  expect 1 "a rustix dependency fails naming it" "depends directly on rustix" "$self" "$d/r"
  sed -i '/^\[dependencies\]/,$d' "$d/r/crates/core/Cargo.toml"
  printf 'pub fn g() { unsafe { libc::unlink(std::ptr::null()); } }\n' >> "$d/r/crates/core/src/lib.rs"
  expect 1 "a planted libc::unlink fails with file and line" "crates/core/src/lib.rs:3" "$self" "$d/r"
  printf '\n[dependencies]\n[dependencies]\n' >> "$d/r/crates/core/Cargo.toml"
  expect 2 "an unparsable manifest fails closed instead of passing" "cargo metadata failed" "$self" "$d/r"
  echo "selftest: $n ok, $bad bad"; [ $bad = 0 ]
}

if [ "${1:-}" = --selftest ]; then selftest; else check "${1:-.}"; fi

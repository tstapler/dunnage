#!/usr/bin/env bash
# check-scan-readonly.sh [repo-root] | --selftest
# The scan crate stays read-only (ADR-007 decision 6): non-test code under crates/scan/src must not name a delete
# primitive. Everything from the first `#[cfg(test)]` line of a file on is treated as test code.
# One exemption (Story 6.1.1): a single line in cache.rs carrying `readonly-ok(cache-temp)` may name a delete primitive
# (the helper that drops the cache's own abandoned temp file); the marker anywhere else, or a second one, fails.
# A missing crates/scan/src, or one with no *.rs file, exits 2: a check that scans nothing must not pass.
set -u -o pipefail
pat='remove_file|remove_dir|unlinkat|libc::unlink|fs::remove'
marker='readonly-ok(cache-temp)'

check() {
  local root=${1:-.} src hits=0 files=0 f out
  src="$root/crates/scan/src"
  [ -d "$src" ] || { echo "check-scan-readonly: $src not found" >&2; return 2; }
  while IFS= read -r f; do
    files=$((files+1))
    out=$(awk -v pat="$pat" -v marker="$marker" -v base="$(basename "$f")" '
      /#\[cfg\(test\)\]/ {exit}
      index($0, marker) { if (base == "cache.rs" && !seen) { seen = 1; next } print FILENAME ":" FNR ": misplaced or repeated " marker ": " $0; next }
      $0 ~ pat {print FILENAME ":" FNR ": " $0}' "$f")
    [ -n "$out" ] && { echo "$out"; hits=$((hits+1)); }
  done < <(find "$src" -name '*.rs' -not -path '*/tests/*')
  [ "$files" -gt 0 ] || { echo "check-scan-readonly: no *.rs under $src" >&2; return 2; }
  if [ $hits = 0 ]; then echo "ok: no delete primitive in non-test scan code ($files files)"; else echo "FAIL: $hits file(s) call a delete primitive outside tests"; fi
  [ $hits = 0 ]
}

selftest() {
  d=""
  local self bad=0 n=0 out rc
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "${d:-}"' EXIT INT TERM
  expect() { local want=$1 name=$2 pat=$3; shift 3; out=$("$@" 2>&1); rc=$?
    if [ "$rc" = "$want" ] && grep -q -- "$pat" <<<"$out"; then n=$((n+1)); echo "ok: $name"; else echo "SELFTEST BAD: $name (rc=$rc): $out"; bad=$((bad+1)); fi; }
  mkdir -p "$d/r/crates/scan/src"
  printf 'pub fn f() {}\n#[cfg(test)]\nmod t { fn g() { std::fs::remove_file("x").ok(); } }\n' > "$d/r/crates/scan/src/lib.rs"
  expect 0 "a delete primitive after #[cfg(test)] is ignored" "^ok:" "$self" "$d/r"
  printf 'pub fn h() { std::fs::remove_file("x").ok(); }\n' > "$d/r/crates/scan/src/walker.rs"
  expect 1 "a planted std::fs::remove_file names file and line" "walker.rs:1:" "$self" "$d/r"
  rm "$d/r/crates/scan/src/walker.rs"
  printf 'pub fn h() { unsafe { libc::unlink(std::ptr::null()); } }\n' > "$d/r/crates/scan/src/walker.rs"
  expect 1 "a planted libc::unlink fails" "walker.rs:1:" "$self" "$d/r"
  rm "$d/r/crates/scan/src/walker.rs"
  printf 'pub fn h() { let _ = rustix::fs::unlinkat; }\n' > "$d/r/crates/scan/src/walker.rs"
  expect 1 "a planted unlinkat fails" "walker.rs:1:" "$self" "$d/r"
  rm -r "$d/r/crates/scan/src"; mkdir "$d/r/crates/scan/src"
  expect 2 "a scan crate with no sources fails closed" "no \\*.rs" "$self" "$d/r"
  expect 2 "a missing crates/scan/src fails closed" "not found" "$self" "$d/empty"
  mkdir -p "$d/m/crates/scan/src"
  printf 'fn a() { let _ = std::fs::remove_file("x"); } // readonly-ok(cache-temp)\n' > "$d/m/crates/scan/src/cache.rs"
  expect 0 "the one marked line in cache.rs passes" "^ok:" "$self" "$d/m"
  printf 'fn a() { let _ = std::fs::remove_file("x"); } // readonly-ok(cache-temp)\nfn b() { let _ = std::fs::remove_file("y"); } // readonly-ok(cache-temp)\n' > "$d/m/crates/scan/src/cache.rs"
  expect 1 "a second marked line in cache.rs fails" "cache.rs:2:" "$self" "$d/m"
  printf 'fn a() { let _ = std::fs::remove_file("x"); } // readonly-ok(cache-temp)\n' > "$d/m/crates/scan/src/cache.rs"
  printf 'fn h() { let _ = std::fs::remove_file("x"); } // readonly-ok(cache-temp)\n' > "$d/m/crates/scan/src/walker.rs"
  expect 1 "the marker in another file fails" "walker.rs:1:" "$self" "$d/m"
  rm "$d/m/crates/scan/src/walker.rs"
  printf 'fn a() { let _ = std::fs::remove_file("x"); } // readonly-ok(cache-temp)\nfn b() { let _ = std::fs::remove_file("y"); }\n' > "$d/m/crates/scan/src/cache.rs"
  expect 1 "an unmarked remove_file in cache.rs fails" "cache.rs:2:" "$self" "$d/m"
  echo "selftest: $n ok, $bad bad"; [ $bad = 0 ]
}

if [ "${1:-}" = --selftest ]; then selftest; else check "${1:-.}"; fi

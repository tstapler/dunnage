#!/usr/bin/env bash
# check-home-sources.sh [repo-root] | --selftest
# Only crates/cli/src/wiring/dirs.rs may read a home directory: no getpwuid, no `home_dir`, no home-directory
# crates, no direct read of the HOME variable anywhere else in the workspace. Comment lines are ignored so docs
# can name the forbidden calls. Also fails when a workspace member depends directly on a home-directory crate.
set -u -o pipefail
ALLOWED=crates/cli/src/wiring/dirs.rs
# Named exceptions, each with a reason; a new entry is a reviewed change.
#  - the hash-pinned adversarial suite (ADR-003, frozen) reads HOME only to place its fixture directory;
#  - the legacy cli main.rs, which Story 1.2.4 replaces with wiring::dirs.
EXCEPTIONS="crates/adapters/tests/delete_adversarial.rs crates/cli/src/main.rs"
forbidden_crates='dirs dirs-next directories etcetera shellexpand'
# shellcheck disable=SC2016
pattern='getpwuid|getpwuid_r|home_dir|dirs_next|directories::|etcetera|shellexpand|User::from_uid|nix::unistd::User|env::var(_os)?[(][[:space:]]*"HOME"'
# `dirs::` is the home-directory crate, but `core` has its own `dirs` module (`crate::dirs::Dirs`), so the crate
# path is matched only as a whole path segment and not inside crates/core (the dependency leg still catches the crate).
dirs_crate='(^|[^[:alnum:]_:])dirs::'

check() {
  local root=${1:-.} bad=0 hits deps dep
  if [ -d "$root/crates" ]; then
    local skip=(! -path "$ALLOWED") e
    for e in $EXCEPTIONS; do skip+=(! -path "$e"); done
    # shellcheck disable=SC2016
    hits=$(cd "$root" && find crates -type f -name '*.rs' "${skip[@]}" -print0 \
      | xargs -0 -r awk -v pat="$pattern" -v dc="$dirs_crate" '
          { line = $0; sub(/^[ \t]*\/\/.*/, "", line)
            if (line ~ pat || (FILENAME !~ /^crates\/core\// && line ~ dc)) printf "%s:%d: %s\n", FILENAME, FNR, $0 }')
    if [ -n "$hits" ]; then echo "FAIL: home directory read outside $ALLOWED:"; echo "$hits"; bad=1; fi
  fi
  if [ -f "$root/Cargo.toml" ]; then
    command -v jq >/dev/null || { echo "check-home-sources: jq is required" >&2; return 2; }
    deps=$(cd "$root" && cargo metadata --no-deps --offline --format-version 1 \
      | jq -r '.packages[].dependencies[].name') || { echo "check-home-sources: cargo metadata failed" >&2; return 2; }
    for dep in $forbidden_crates; do
      if grep -qx "$dep" <<<"$deps"; then echo "FAIL: a workspace member depends directly on $dep"; bad=1; fi
    done
  fi
  [ $bad = 0 ] && echo "ok: no home directory read outside $ALLOWED"
  return $bad
}

selftest() {
  d=""
  local self bad=0 n=0 out rc
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "${d:-}"' EXIT INT TERM
  expect() { local want=$1 name=$2 pat=$3; shift 3; out=$("$@" 2>&1); rc=$?
    if [ "$rc" = "$want" ] && grep -q -- "$pat" <<<"$out"; then n=$((n+1)); echo "ok: $name"; else echo "SELFTEST BAD: $name (rc=$rc): $out"; bad=$((bad+1)); fi; }
  mkdir -p "$d/r/crates/cli/src/wiring" "$d/r/crates/core/src"
  printf '[workspace]\nresolver = "2"\nmembers = ["crates/core"]\n' > "$d/r/Cargo.toml"
  printf '[package]\nname = "c"\nversion = "0.1.0"\nedition = "2021"\n' > "$d/r/crates/core/Cargo.toml"
  printf '//! docs may name env::var_os("HOME") and getpwuid in a comment\npub fn f() {}\n' > "$d/r/crates/core/src/lib.rs"
  printf 'fn home() { let _ = std::env::var_os("HOME"); let _ = getpwuid(0); }\n' > "$d/r/crates/cli/src/wiring/dirs.rs"
  expect 0 "clean fixture passes (the wiring file may read HOME; comments are ignored)" "^ok:" "$self" "$d/r"
  printf 'pub fn g() { let _ = std::env::home_dir(); }\n' >> "$d/r/crates/core/src/lib.rs"
  expect 1 "a planted std::env::home_dir() fails with file and line" "crates/core/src/lib.rs:3" "$self" "$d/r"
  printf '//! docs\npub fn f() {}\npub fn g() { let _ = std::env::var_os("HOME"); }\n' > "$d/r/crates/core/src/lib.rs"
  expect 1 "a planted env::var_os(\"HOME\") fails" "outside crates/cli/src/wiring/dirs.rs" "$self" "$d/r"
  printf 'pub fn g() { let _ = directories::BaseDirs::new(); }\n' > "$d/r/crates/core/src/lib.rs"
  expect 1 "a planted directories::BaseDirs::new() fails" "lib.rs:1" "$self" "$d/r"
  printf 'pub fn g() { unsafe { libc::getpwuid(0); } }\n' > "$d/r/crates/core/src/lib.rs"
  expect 1 "a planted getpwuid fails" "lib.rs:1" "$self" "$d/r"
  printf 'pub fn g() { let _ = std::env::var("HOME"); }\n' > "$d/r/crates/core/src/lib.rs"
  expect 1 "a planted env::var(\"HOME\") fails" "lib.rs:1" "$self" "$d/r"
  printf 'pub fn f() {}\n' > "$d/r/crates/core/src/lib.rs"
  printf 'pub fn g() { let _ = dirs::home_dir(); }\n' > "$d/r/crates/cli/src/x.rs"
  expect 1 "a planted dirs::home_dir() outside core fails" "x.rs:1" "$self" "$d/r"
  printf 'pub fn g() { let _ = dirs::cache_dir(); }\n' > "$d/r/crates/cli/src/x.rs"
  expect 1 "a planted dirs::cache_dir() outside core fails" "x.rs:1" "$self" "$d/r"
  rm "$d/r/crates/cli/src/x.rs"
  printf 'pub mod dirs;\npub use dirs::Dirs;\nuse crate::dirs::Dirs as D;\n' > "$d/r/crates/core/src/lib.rs"
  expect 0 "core's own dirs module is not the dirs crate" "^ok:" "$self" "$d/r"
  mkdir -p "$d/r/crates/adapters/tests"; printf 'fn h() { let _ = std::env::var_os("HOME"); }\n' > "$d/r/crates/adapters/tests/delete_adversarial.rs"
  expect 0 "the named exception passes" "^ok:" "$self" "$d/r"
  printf 'fn h() { let _ = std::env::var_os("HOME"); }\n' > "$d/r/crates/adapters/tests/other.rs"
  expect 1 "the same read in another file fails" "other.rs:1" "$self" "$d/r"
  rm "$d/r/crates/adapters/tests/other.rs"
  printf 'pub fn f() {}\n' > "$d/r/crates/core/src/lib.rs"
  printf '\n[dependencies]\ndirs = "5"\n' >> "$d/r/crates/core/Cargo.toml"
  expect 1 "a direct dirs dependency fails naming it" "depends directly on dirs" "$self" "$d/r"
  echo "selftest: $n ok, $bad bad"; [ $bad = 0 ]
}

if [ "${1:-}" = --selftest ]; then selftest; else check "${1:-.}"; fi

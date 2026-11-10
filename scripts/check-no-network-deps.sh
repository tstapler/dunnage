#!/usr/bin/env bash
# check-no-network-deps.sh [repo-root] | --selftest
# No HTTP/socket crate may be a direct dependency of a workspace member or appear in disk-cleanup's resolved
# tree (disk-cleanup-scan's transitive tree included), and no `std::net` use may exist under crates/.
set -u -o pipefail
net_crates='reqwest ureq hyper curl isahc attohttpc surf tokio async-std'

check() {
  local root=${1:-.} bad=0 direct tree c hits line
  command -v jq >/dev/null || { echo "check-no-network-deps: jq is required" >&2; return 2; }
  direct=$(cd "$root" && cargo metadata --no-deps --offline --format-version 1 \
    | jq -r '.packages[] | .name as $p | .dependencies[] | "\(.name) \($p)"') \
    || { echo "check-no-network-deps: cargo metadata failed" >&2; return 2; }
  tree=$(cd "$root" && cargo tree --offline -e normal --prefix none -p disk-cleanup 2>/dev/null | awk '{print $1}' | sort -u)
  for c in $net_crates; do
    while read -r dep member; do
      [ -n "$dep" ] || continue
      echo "FAIL: workspace member $member depends directly on $dep"; bad=1
    done < <(grep -E "^$c " <<<"$direct")
    if grep -qx "$c" <<<"$tree"; then echo "FAIL: $c is in the resolved tree of disk-cleanup"; bad=1; fi
  done
  if [ -d "$root/crates" ]; then
    hits=$(cd "$root" && find crates -type f -name '*.rs' -print0 | xargs -0 -r grep -nH 'std::net')
    if [ -n "$hits" ]; then echo "FAIL: std::net use:"; echo "$hits"; bad=1; fi
  fi
  line="ok: no network dependency or std::net use"
  [ $bad = 0 ] && echo "$line"
  return $bad
}

selftest() {
  local self bad=0 n=0 out rc
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "${d:?}"' EXIT INT TERM
  expect() { local want=$1 name=$2 pat=$3; shift 3; out=$("$@" 2>&1); rc=$?
    if [ "$rc" = "$want" ] && grep -q -- "$pat" <<<"$out"; then n=$((n+1)); echo "ok: $name"; else echo "SELFTEST BAD: $name (rc=$rc): $out"; bad=$((bad+1)); fi; }
  mkdir -p "$d/r/crates/app/src"
  printf '[workspace]\nresolver = "2"\nmembers = ["crates/app"]\n' > "$d/r/Cargo.toml"
  printf '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n' > "$d/r/crates/app/Cargo.toml"
  printf 'fn main() {}\n' > "$d/r/crates/app/src/main.rs"
  expect 0 "clean fixture passes" "^ok:" "$self" "$d/r"
  printf '\n[dependencies]\nreqwest = "0.12"\n' >> "$d/r/crates/app/Cargo.toml"
  expect 1 "a reqwest dependency fails naming it" "depends directly on reqwest" "$self" "$d/r"
  sed -i '/^\[dependencies\]/,$d' "$d/r/crates/app/Cargo.toml"
  printf 'fn main() { let _ = std::net::TcpStream::connect("127.0.0.1:1"); }\n' > "$d/r/crates/app/src/main.rs"
  expect 1 "a std::net::TcpStream use fails naming the file" "crates/app/src/main.rs" "$self" "$d/r"
  printf '\n[dependencies]\n[dependencies]\n' >> "$d/r/crates/app/Cargo.toml"
  expect 2 "an unparsable manifest fails closed instead of passing" "cargo metadata failed" "$self" "$d/r"
  echo "selftest: $n ok, $bad bad"; [ $bad = 0 ]
}

if [ "${1:-}" = --selftest ]; then selftest; else check "${1:-.}"; fi

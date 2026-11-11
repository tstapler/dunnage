#!/usr/bin/env bash
# check-safety-manifest.sh [repo-root] [--gate A-F] | --selftest
# The safety list and the safety tests must agree, so a renamed or deleted `safety_` test cannot go unnoticed:
#   - every guarantee listed for the active gate or earlier names a `fn <name>` under crates/ (else exit 1 naming it);
#   - every `fn safety_*` under crates/ is listed (else exit 1 naming it).
# Lists read: ci/safety/manifest.txt (`<first-gate> <test_name>  # guarantee`, plus `active-gate <A-F>`),
# ci/safety-guarantees.txt and ci/safety/<story>.txt (one test name per line, always active). `#` starts a comment.
# Exit 2 if a list or crates/ is missing or unreadable (never a silent pass). Story 2.3.5, Task 2.3.5b.
# shellcheck disable=SC2295
set -u -o pipefail
order="ABCDEF"
rank() { local p=${order%%$1*}; echo ${#p}; }

check() {
  local root=$1 gate=${2:-} listed tests name first bad=0 f line
  [ -d "$root/crates" ] || { echo "check-safety-manifest: no crates/ under $root" >&2; return 2; }
  [ -r "$root/ci/safety/manifest.txt" ] || { echo "check-safety-manifest: ci/safety/manifest.txt is missing" >&2; return 2; }
  if [ -z "$gate" ]; then
    gate=$(awk '$1 == "active-gate" { print $2; exit }' "$root/ci/safety/manifest.txt")
  fi
  case $gate in [A-F]) ;; *) echo "check-safety-manifest: no valid active-gate line (A-F) in ci/safety/manifest.txt" >&2; return 2 ;; esac
  listed=$(mktemp); tests=$(mktemp)
  # shellcheck disable=SC2064
  trap "rm -f '$listed' '$tests'" RETURN
  while read -r first name _; do
    [ -n "${first:-}" ] && [ "${first:0:1}" != "#" ] && [ "$first" != active-gate ] || continue
    case $first in [A-F]) ;; *) echo "FAIL: ci/safety/manifest.txt: bad line '$first ${name:-}' (want '<gate A-F> <test_name>')"; bad=1; continue ;; esac
    [ -n "${name:-}" ] || { echo "FAIL: ci/safety/manifest.txt: gate $first without a test name"; bad=1; continue; }
    [ "$(rank "$first")" -le "$(rank "$gate")" ] && echo "$name" >>"$listed"
  done <"$root/ci/safety/manifest.txt"
  for f in "$root/ci/safety-guarantees.txt" "$root"/ci/safety/*.txt; do
    [ -r "$f" ] || continue
    case ${f##*/} in manifest.txt | README.txt) continue ;; esac
    while read -r line; do
      line=${line%%#*}; read -r name _ <<<"$line"
      [ -n "${name:-}" ] && echo "$name" >>"$listed"
    done <"$f"
  done
  # every safety_ fn under crates/; comments and strings that merely mention a name do not count
  grep -rhoE --include='*.rs' '^[[:space:]]*(pub(\([a-z]+\))? )?(async )?fn safety_[A-Za-z0-9_]*' "$root/crates" \
    | sed -E 's/.*fn //' | sort -u >"$tests" || true
  sort -u -o "$listed" "$listed"
  while read -r name; do
    [ -n "$name" ] || continue
    grep -rqE --include='*.rs' "^[[:space:]]*(pub(\([a-z]+\))? )?(async )?fn $name\b" "$root/crates" \
      || { echo "FAIL: listed guarantee '$name' has no test: no 'fn $name' under crates/"; bad=1; }
  done <"$listed"
  while read -r name; do
    [ -n "$name" ] || continue
    grep -qx "$name" "$listed" || { echo "FAIL: test '$name' is not listed in ci/safety/manifest.txt, ci/safety-guarantees.txt or ci/safety/<story>.txt"; bad=1; }
  done <"$tests"
  [ $bad = 0 ] && echo "ok: $(wc -l <"$listed" | tr -d ' ') listed guarantees (gate $gate) and $(wc -l <"$tests" | tr -d ' ') safety_ tests agree"
  return $bad
}

selftest() {
  local self bad=0 n=0 out rc
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "${d:?}"' EXIT INT TERM
  expect() { local want=$1 name=$2 pat=$3; shift 3; out=$("$@" 2>&1); rc=$?
    if [ "$rc" = "$want" ] && grep -qF -- "$pat" <<<"$out"; then n=$((n+1)); echo "ok: $name"; else bad=$((bad+1)); echo "SELFTEST BAD: $name (rc=$rc, want $want): $out"; fi; }
  mkdir -p "$d/crates/a/tests" "$d/ci/safety"
  printf '# manifest\nactive-gate B\nA safety_one  # first guarantee\nB safety_two\nC safety_three\n' >"$d/ci/safety/manifest.txt"
  printf '# names\n' >"$d/ci/safety-guarantees.txt"
  printf '#[test]\nfn safety_one() {}\n#[test]\nfn safety_two() {}\n' >"$d/crates/a/tests/s.rs"
  expect 0 "list and tests agree; a guarantee of a later gate is not required yet" "ok: 2 listed guarantees (gate B)" "$self" "$d"
  expect 1 "the same tree at gate C requires the gate C guarantee and names it" "listed guarantee 'safety_three' has no test" "$self" "$d" --gate C
  printf '#[test]\nfn safety_three() {}\n' >>"$d/crates/a/tests/s.rs"
  expect 0 "adding the test satisfies gate C" "ok: 3 listed guarantees (gate C)" "$self" "$d" --gate C
  sed -i '/safety_three/d' "$d/crates/a/tests/s.rs"
  sed -i 's/fn safety_two/fn safety_twoo/' "$d/crates/a/tests/s.rs"
  expect 1 "a renamed test: the listed name has no test" "listed guarantee 'safety_two' has no test" "$self" "$d"
  expect 1 "a renamed test: the new name is not listed" "test 'safety_twoo' is not listed" "$self" "$d"
  sed -i 's/safety_twoo/safety_two/' "$d/crates/a/tests/s.rs"
  printf '#[test]\nfn safety_extra() {}\n' >"$d/crates/a/tests/extra.rs"
  expect 1 "an unlisted safety_ test is named" "test 'safety_extra' is not listed" "$self" "$d"
  printf 'safety_extra  # story 9.9\n' >"$d/ci/safety/9.9.txt"
  expect 0 "a per-story list entry lists it" "ok:" "$self" "$d"
  rm "$d/ci/safety/9.9.txt"; printf 'safety_extra\n' >"$d/ci/safety-guarantees.txt"
  expect 0 "ci/safety-guarantees.txt lists it too" "ok:" "$self" "$d"
  printf '// fn safety_ghost() {}\nlet s = "fn safety_ghost";\n' >"$d/crates/a/tests/c.rs"
  expect 0 "a comment or string that mentions a name is not a test" "ok:" "$self" "$d"
  printf 'safety_ghost\n' >>"$d/ci/safety-guarantees.txt"
  expect 1 "a name that only a comment mentions is not a test" "listed guarantee 'safety_ghost' has no test" "$self" "$d"
  printf 'safety_extra\n' >"$d/ci/safety-guarantees.txt"; rm "$d/crates/a/tests/c.rs"
  printf 'X safety_bad\n' >>"$d/ci/safety/manifest.txt"
  expect 1 "a malformed manifest line fails" "bad line" "$self" "$d"
  printf '# manifest\nA safety_one\n' >"$d/ci/safety/manifest.txt"
  expect 2 "no active-gate line exits 2, never a pass" "no valid active-gate" "$self" "$d"
  rm "$d/ci/safety/manifest.txt"
  expect 2 "a missing manifest exits 2" "manifest.txt is missing" "$self" "$d"
  expect 2 "a root without crates/ exits 2" "no crates/" "$self" "$d/ci"
  echo "selftest: $n ok, $bad bad"; [ $bad = 0 ]
}

if [ "${1:-}" = --selftest ]; then selftest
else
  root=.; gate=""
  while [ $# -gt 0 ]; do case $1 in --gate) gate=${2:?}; shift 2 ;; *) root=$1; shift ;; esac; done
  check "$root" "$gate"
fi

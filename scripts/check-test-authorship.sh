#!/usr/bin/env bash
# check-test-authorship.sh [repo-root] | --selftest
# The adversarial suites are trusted only if
#   (1) ci/delete_adversarial.sha256 lists both pinned files and `sha256sum -c` passes on them, and
#   (2) no commit changed a pinned test file together with crates/adapters/src/delete.rs or crates/core/src/floor_rule.rs.
# A change to a pinned file is a commit of its own that also updates the pin ("test-author:" message). There is no
# signature: the pin is an in-repo guard, and gate.sh prints the pin diff since the previous gate report (ADR-003 amended).
set -u
PIN=ci/delete_adversarial.sha256
PINNED="crates/adapters/tests/delete_adversarial.rs crates/core/tests/floor_rule.rs"
ENGINES="crates/adapters/src/delete.rs crates/core/src/floor_rule.rs"

check() {
  local root=$1 rc=0 f e
  cd "$root" || { echo "FAIL: no such repo $root"; return 2; }
  for t in git sha256sum; do command -v "$t" >/dev/null || { echo "FAIL: required tool $t not found"; return 2; }; done
  [ -f "$PIN" ] || { echo "FAIL: $PIN missing"; return 2; }
  for f in $PINNED; do
    grep -qE "^[0-9a-f]{64}  $f\$" "$PIN" || { echo "FAIL: $PIN does not pin $f"; rc=1; }
  done
  sha256sum --quiet -c "$PIN" 2>/dev/null || { echo "FAIL: a pinned test file differs from $PIN"; rc=1; }
  local pin_commit
  pin_commit=$(git log -1 --format=%H -- "$PIN")
  [ -n "$pin_commit" ] || { echo "FAIL: $PIN is not committed"; return 1; }
  [ -z "$(git status --porcelain -- "$PIN" $PINNED)" ] || { echo "FAIL: pin or pinned test files have uncommitted changes"; rc=1; }
  local c
  for f in $PINNED; do
    for c in $(git log --format=%H -- "$f"); do
      for e in $ENGINES; do
        if git diff-tree --no-commit-id --name-only -r "$c" | grep -qx "$e"; then
          echo "FAIL: commit ${c:0:10} changes $f together with $e"; rc=1
        fi
      done
    done
  done
  [ $rc = 0 ] && echo "ok: pin and commit separation hold"
  return $rc
}

selftest() {
  local self bad=0 n=0
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "$d"' EXIT INT TERM
  expect() { local want=$1 name=$2; shift 2; local out rc; out=$("$@" 2>&1); rc=$?
    if { [ "$want" = pass ] && [ $rc = 0 ]; } || { [ "$want" = fail ] && [ $rc = 1 ]; } || { [ "$want" = usage ] && [ $rc = 2 ]; }; then n=$((n+1)); echo "ok: $name"; else echo "SELFTEST BAD: $name (rc=$rc): $out"; bad=$((bad+1)); fi; }
  local T1=crates/adapters/tests/delete_adversarial.rs T2=crates/core/tests/floor_rule.rs E1=crates/adapters/src/delete.rs E2=crates/core/src/floor_rule.rs
  mk() { rm -rf "$d/r"; mkdir -p "$d/r/ci" "$d/r/crates/adapters/src" "$d/r/crates/adapters/tests" "$d/r/crates/core/src" "$d/r/crates/core/tests"; cd "$d/r" || exit 2
    git init -q -b main; git config user.email t@t; git config user.name t; git config commit.gpgsign false
    echo 'fn a(){}' > $E1; echo 'fn f(){}' > $E2; echo '#[test] fn t(){}' > $T1; echo '#[test] fn u(){}' > $T2; }
  pin() { sha256sum $T1 $T2 > $PIN; }
  mk; pin; git add -A; git commit -qm 'test-author: suite'
  expect pass "pinned suite, no engine change in the same commit" "$self" "$d/r"
  echo '#[test] fn weak(){}' > $T1; git commit -qam 'weaken'
  expect fail "test file edited after the pin (hash differs)" "$self" "$d/r"
  pin; git commit -qam 'test-author: repin'
  expect pass "pin updated in its own commit" "$self" "$d/r"
  echo 'fn b(){}' > $E1; echo '#[test] fn t2(){}' > $T1; pin; git commit -qam 'both files'
  expect fail "one commit changes engine and test file together" "$self" "$d/r"
  mk; pin; git add -A; git commit -qm suite
  sha256sum $T1 > $PIN; git commit -qam 'drop floor_rule.rs from the pin'
  expect fail "pin that omits one of the two pinned files" "$self" "$d/r"
  mk; pin; git add -A; git commit -qm suite
  echo '#[test] fn uncommitted(){}' > $T2
  expect fail "uncommitted edit of a pinned file" "$self" "$d/r"
  mk; pin; git add -A; git commit -qm suite; git rm -q $PIN; git commit -qm 'drop pin'
  expect usage "missing pin file exits 2" "$self" "$d/r"
  echo "selftest: $n ok, $bad bad"; [ $bad = 0 ]
}

case "${1:-.}" in --selftest) selftest ;; *) check "${1:-.}" ;; esac

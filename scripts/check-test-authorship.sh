#!/usr/bin/env bash
# check-test-authorship.sh [repo-root] | --selftest
# The adversarial suites are trusted only if
#   (1) ci/delete_adversarial.sha256 lists both pinned files and `sha256sum -c` passes on them,
#   (2) an annotated tag test-author-v<N>, signed by a key in ci/owner-signers, points at the commit that last
#       changed the pin (a "test-author:" commit label, a lightweight tag or an unsigned tag count for nothing),
#   (3) no commit changed a pinned test file together with crates/adapters/src/delete.rs or crates/core/src/floor_rule.rs,
#   (4) ci/owner-signers has at least one key and no commit after the pin commit touched it (the owner adds the key
#       in the same commit that re-pins; a later edit cannot widen who may sign). The control is only as strong as the
#       custody of the signing key: it must not be readable by an implementing session (not an unencrypted ~/.ssh key).
# The tag is created by the owner only: git -c gpg.format=ssh -c user.signingkey=<key> tag -s test-author-v<N> <pin-commit>
set -u
PIN=ci/delete_adversarial.sha256
SIGNERS=ci/owner-signers
PINNED="crates/adapters/tests/delete_adversarial.rs crates/core/tests/floor_rule.rs"
ENGINES="crates/adapters/src/delete.rs crates/core/src/floor_rule.rs"

check() {
  local root=$1 rc=0 f e
  cd "$root" || { echo "FAIL: no such repo $root"; return 2; }
  for t in git sha256sum ssh-keygen; do command -v "$t" >/dev/null || { echo "FAIL: required tool $t not found"; return 2; }; done
  [ -f "$PIN" ] && [ -f "$SIGNERS" ] || { echo "FAIL: $PIN or $SIGNERS missing"; return 1; }
  for f in $PINNED; do
    grep -qE "^[0-9a-f]{64}  $f\$" "$PIN" || { echo "FAIL: $PIN does not pin $f"; rc=1; }
  done
  sha256sum --quiet -c "$PIN" 2>/dev/null || { echo "FAIL: a pinned test file differs from $PIN"; rc=1; }
  local pin_commit tag found=0
  pin_commit=$(git log -1 --format=%H -- "$PIN")
  [ -n "$pin_commit" ] || { echo "FAIL: $PIN is not committed"; return 1; }
  [ -z "$(git status --porcelain -- "$PIN" "$SIGNERS" $PINNED)" ] || { echo "FAIL: pin, signers or pinned test files have uncommitted changes"; rc=1; }
  grep -qE '^[^#[:space:]]' "$SIGNERS" || { echo "FAIL: $SIGNERS holds no signer key yet (the owner adds a dedicated signing key's public half)"; rc=1; }
  [ -z "$(git log --format=%H "$pin_commit..HEAD" -- "$SIGNERS")" ] || { echo "FAIL: $SIGNERS was edited after the pin commit ${pin_commit:0:10}"; rc=1; }
  for tag in $(git tag --list 'test-author-v*'); do
    [ "$(git cat-file -t "refs/tags/$tag")" = tag ] || continue                    # lightweight tags do not count
    [ "$(git rev-parse "$tag^{commit}")" = "$pin_commit" ] || continue              # must point at the pin commit itself
    if git -c gpg.format=ssh -c gpg.ssh.allowedSignersFile="$SIGNERS" verify-tag "$tag" >/dev/null 2>&1; then found=1; fi
  done
  [ $found = 1 ] || { echo "FAIL: no owner-signed annotated tag test-author-v* on pin commit ${pin_commit:0:10}"; rc=1; }
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
  [ $rc = 0 ] && echo "ok: pin, signed tag and commit separation hold"
  return $rc
}

selftest() {
  local self bad=0 n=0
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "$d"' EXIT INT TERM
  command -v ssh-keygen >/dev/null || { echo "SELFTEST BAD: ssh-keygen not found"; return 2; }
  ssh-keygen -q -t ed25519 -N '' -f "$d/owner" -C owner; ssh-keygen -q -t ed25519 -N '' -f "$d/agent" -C agent
  expect() { local want=$1 name=$2; shift 2; local out rc; out=$("$@" 2>&1); rc=$?
    if { [ "$want" = pass ] && [ $rc = 0 ]; } || { [ "$want" = fail ] && [ $rc = 1 ]; }; then n=$((n+1)); echo "ok: $name"; else echo "SELFTEST BAD: $name (rc=$rc): $out"; bad=$((bad+1)); fi; }
  local T1=crates/adapters/tests/delete_adversarial.rs T2=crates/core/tests/floor_rule.rs E1=crates/adapters/src/delete.rs E2=crates/core/src/floor_rule.rs
  mk() { rm -rf "$d/r"; mkdir -p "$d/r/ci" "$d/r/crates/adapters/src" "$d/r/crates/adapters/tests" "$d/r/crates/core/src" "$d/r/crates/core/tests"; cd "$d/r" || exit 2
    git init -q -b main; git config user.email t@t; git config user.name t; git config commit.gpgsign false
    echo 'fn a(){}' > $E1; echo 'fn f(){}' > $E2; echo '#[test] fn t(){}' > $T1; echo '#[test] fn u(){}' > $T2
    printf 'owner %s\n' "$(cut -d' ' -f1,2 "$d/owner.pub")" > $SIGNERS; }
  pin() { sha256sum $T1 $T2 > $PIN; }
  signed_tag() { git -c gpg.format=ssh -c user.signingkey="$1" tag -s "$2" -m "$2"; }
  mk; pin; git add -A; git commit -qm 'test-author: suite'
  expect fail "no tag at all (the state before the owner signs)" "$self" "$d/r"
  signed_tag "$d/owner" test-author-v1
  expect pass "owner-signed tag on the pin commit" "$self" "$d/r"
  echo '#[test] fn weak(){}' > $T1; git commit -qam 'weaken'
  expect fail "test file edited after the pin (hash differs)" "$self" "$d/r"
  pin; git commit -qam 'test-author: repin (label only, no tag)'
  expect fail "label-only commit, tag is on an older commit" "$self" "$d/r"
  signed_tag "$d/agent" test-author-v2
  expect fail "tag signed by a key that is not an owner key" "$self" "$d/r"
  git tag -d test-author-v2 >/dev/null; git tag -a test-author-v2 -m unsigned
  expect fail "annotated but unsigned tag" "$self" "$d/r"
  git tag -d test-author-v2 >/dev/null; git tag test-author-v2
  expect fail "lightweight tag" "$self" "$d/r"
  git tag -d test-author-v2 >/dev/null; signed_tag "$d/owner" test-author-v2
  expect pass "owner-signed tag re-applied to the new pin commit" "$self" "$d/r"
  echo 'fn b(){}' > $E1; echo '#[test] fn t2(){}' > $T1; pin; git commit -qam 'both files'
  git tag -d test-author-v2 >/dev/null; signed_tag "$d/owner" test-author-v3
  expect fail "one commit changes engine and test file together" "$self" "$d/r"
  mk; pin; git add -A; git commit -qm suite; signed_tag "$d/owner" test-author-v1
  sha256sum $T1 > $PIN; git commit -qam 'drop floor_rule.rs from the pin'; git tag -d test-author-v1 >/dev/null; signed_tag "$d/owner" test-author-v1
  expect fail "pin that omits one of the two pinned files" "$self" "$d/r"
  mk; pin; git add -A; git commit -qm suite; signed_tag "$d/owner" test-author-v1
  printf 'agent %s\n' "$(cut -d' ' -f1,2 "$d/agent.pub")" >> $SIGNERS; git commit -qam 'add a signer'
  expect fail "owner-signers edited after the pin commit" "$self" "$d/r"
  mk; printf '# no key yet\n' > $SIGNERS; pin; git add -A; git commit -qm suite; signed_tag "$d/owner" test-author-v1
  expect fail "owner-signers holds no key" "$self" "$d/r"
  mk; pin; git add -A; git commit -qm suite; signed_tag "$d/owner" test-author-v1
  echo '#[test] fn uncommitted(){}' > $T2
  expect fail "uncommitted edit of a pinned file" "$self" "$d/r"
  echo "selftest: $n ok, $bad bad"; [ $bad = 0 ]
}

case "${1:-.}" in --selftest) selftest ;; *) check "${1:-.}" ;; esac

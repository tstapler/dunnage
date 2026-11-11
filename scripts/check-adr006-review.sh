#!/usr/bin/env bash
# check-adr006-review.sh | --selftest
# Story 6.6.3 starts only on a fresh, accepted architecture review of the CURRENT ADR-006 (Tasks 6.6.2c and 6.6.2e).
# Reads the newest review `explorer-deletion-architecture-review[-N].md` in DC_ADR_DIR (the planning directory lives in
# the dotfiles repo, not here). Its first three lines must be
#   VERDICT: PASS|CONCERNS|BLOCK / reviewed-adr-006-sha256: <hash> / reviewed-commit: <sha>
# and optionally (within the first 8 lines) `reviewed-rule-table-sha256:` and `reviewed-apply-order-sha256:`, the hashes of
# the plan.md text of Story 6.6.1 (rules M1 to M8) and of the "The order of the apply" bullet of Story 6.6.3; when
# present they are compared like the ADR hash, when absent that part is not checked (the review format has no slot).
# Outcome: STALE (exit 1) if any recorded hash differs from the current text; BLOCK, a malformed header or no review
# fails; PASS passes; CONCERNS passes only if explorer-deletion-review-response.md exists, has at least one
# `| C<n> ...` table row and no row ends a cell in OPEN or UNRESOLVED. The response covers the first review, so a second
# review that says CONCERNS passes on the same rule: an open row, not the file name, is what blocks it.
# Env: DC_ADR_DIR (default ~/dotfiles/project_plans/rust-disk-cleaner/implementation), DC_ADR006 (default
# $DC_ADR_DIR/../decisions/ADR-006-explorer-deletion.md), DC_PLAN (default $DC_ADR_DIR/plan.md).
# DC_REQUIRE_ADR_REVIEW=1 (gate.sh sets it) makes an absent DC_ADR_DIR a failure; unset, it prints SKIP and exits 0,
# because hosted CI runners have no dotfiles checkout.
set -u

hash_of() { sha256sum | cut -d' ' -f1; }
header() { sed -n "$2" "$1" | tr -d '\r'; }   # header FILE SED-ADDRESS
section() { # section PLAN START-REGEX END-REGEX: lines from the first START match up to (not including) the next END match
  awk -v s="$2" -v e="$3" '$0 ~ s && !on {on=1; print; next} on && $0 ~ e {exit} on {print}' "$1"
}

check() {
  local dir=${DC_ADR_DIR:-$HOME/dotfiles/project_plans/rust-disk-cleaner/implementation}
  local adr=${DC_ADR006:-$dir/../decisions/ADR-006-explorer-deletion.md} plan=${DC_PLAN:-$dir/plan.md}
  local review resp verdict want have ln1 ln2 ln3 name opts
  if [ ! -d "$dir" ]; then
    if [ "${DC_REQUIRE_ADR_REVIEW:-0}" = 1 ]; then echo "FAIL: planning directory $dir not found (DC_REQUIRE_ADR_REVIEW=1)"; return 1; fi
    echo "SKIP: planning directory $dir not found (set DC_ADR_DIR; DC_REQUIRE_ADR_REVIEW=1 makes this a failure)"; return 0
  fi
  [ -f "$adr" ] || { echo "FAIL: ADR-006 not found at $adr (set DC_ADR006)"; return 1; }
  review=$(printf '%s\n' "$dir"/explorer-deletion-architecture-review.md "$dir"/explorer-deletion-architecture-review-[0-9]*.md \
    | while IFS= read -r f; do [ -f "$f" ] && echo "$f"; done | sort -V | tail -1)
  [ -n "$review" ] || { echo "FAIL: no accepted review covers the current ADR-006: $dir has no explorer-deletion-architecture-review*.md (run Task 6.6.2c or 6.6.2e with a fresh agent)"; return 1; }
  name=${review##*/}
  ln1=$(header "$review" 1p) ln2=$(header "$review" 2p) ln3=$(header "$review" 3p)
  [[ $ln1 =~ ^VERDICT:\ (PASS|CONCERNS|BLOCK)$ ]] || { echo "FAIL: $name line 1 is not 'VERDICT: PASS|CONCERNS|BLOCK': '$ln1'"; return 1; }
  verdict=${BASH_REMATCH[1]}
  [[ $ln2 =~ ^reviewed-adr-006-sha256:\ ([0-9a-f]{64})$ ]] || { echo "FAIL: $name line 2 is not 'reviewed-adr-006-sha256: <64 hex>': '$ln2'"; return 1; }
  want=${BASH_REMATCH[1]}
  [[ $ln3 =~ ^reviewed-commit:\ [0-9a-f]{7,40}$ ]] || { echo "FAIL: $name line 3 is not 'reviewed-commit: <sha>': '$ln3'"; return 1; }
  have=$(hash_of < "$adr")
  if [ "$want" != "$have" ]; then
    echo "explorer-deletion review is STALE: re-run Task 6.6.2c with a fresh agent ($name reviewed ADR-006 ${want:0:12}, the current ADR-006 is ${have:0:12})"; return 1
  fi
  opts=$(header "$review" 1,8p)
  for kind in rule-table apply-order; do
    want=$(grep -oE "^reviewed-$kind-sha256: [0-9a-f]{64}" <<<"$opts" | cut -d' ' -f2)
    [ -n "$want" ] || { echo "note: $name records no reviewed-$kind-sha256 (not checked)"; continue; }
    [ -f "$plan" ] || { echo "FAIL: $name records reviewed-$kind-sha256 but $plan is missing (set DC_PLAN)"; return 1; }
    if [ $kind = rule-table ]; then have=$(section "$plan" '^#### Story 6\.6\.1:' '^#### Story ' | hash_of)
    else have=$(section "$plan" '^- The order of the apply' '^- ' | hash_of); fi
    if [ "$want" != "$have" ]; then
      echo "explorer-deletion review is STALE: re-run Task 6.6.2c with a fresh agent ($name reviewed a different $kind text in the plan)"; return 1
    fi
  done
  case $verdict in
    BLOCK) echo "FAIL: $name says VERDICT: BLOCK: fix the blockers and repeat the review with a different fresh agent"; return 1 ;;
    PASS) echo "ok: $name PASS covers the current ADR-006 (${have:0:12})" ;;
    CONCERNS)
      resp=$dir/explorer-deletion-review-response.md
      [ -f "$resp" ] || { echo "FAIL: $name says CONCERNS and $resp does not exist: the concerns are not accepted"; return 1; }
      grep -qE '^\| *C[0-9]+ ' "$resp" || { echo "FAIL: $resp has no '| C<n> ...' concern rows"; return 1; }
      if grep -nE '^\| *C[0-9]+ .*\| *(OPEN|UNRESOLVED) *\|' "$resp"; then echo "FAIL: $resp has open concerns (rows above)"; return 1; fi
      echo "ok: $name CONCERNS accepted: $(grep -cE '^\| *C[0-9]+ ' "$resp") concern rows in the response, none open" ;;
  esac
}

selftest() {
  local self n=0 bad=0 out rc h
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "$d"' EXIT INT TERM
  mkdir -p "$d/impl" "$d/decisions"
  printf 'decision one\n' > "$d/decisions/ADR-006-explorer-deletion.md"
  h=$(sha256sum < "$d/decisions/ADR-006-explorer-deletion.md" | cut -d' ' -f1)
  # run NAME WANT-RC PATTERN [ENV...]: runs the check against $d with a clean environment
  run() { local name=$1 want=$2 pat=$3; shift 3
    out=$(env -i PATH="$PATH" HOME="$d/nohome" DC_ADR_DIR="$d/impl" DC_ADR006="$d/decisions/ADR-006-explorer-deletion.md" "$@" bash "$self" 2>&1); rc=$?
    if [ "$rc" = "$want" ] && grep -q -- "$pat" <<<"$out"; then n=$((n+1)); echo "ok: $name"; else echo "SELFTEST BAD: $name (rc=$rc, want $want, pattern '$pat'): $out"; bad=$((bad+1)); fi; }
  review() { printf 'VERDICT: %s\nreviewed-adr-006-sha256: %s\nreviewed-commit: abc1234\n\n# review\n' "$1" "$2" > "$d/impl/explorer-deletion-architecture-review.md"; }
  rm -f "$d/impl"/*
  run "missing review fails with a clear message" 1 "no accepted review covers the current ADR-006"
  review PASS "$h"
  run "PASS on the fresh ADR passes" 0 "^ok: .*PASS covers"
  printf 'decision one, amended\n' > "$d/decisions/ADR-006-explorer-deletion.md"
  run "PASS on a changed ADR (one edit) is STALE" 1 "review is STALE: re-run Task 6.6.2c with a fresh agent"
  printf 'decision one\n' > "$d/decisions/ADR-006-explorer-deletion.md"
  review CONCERNS "$h"
  run "CONCERNS without a response file fails" 1 "does not exist: the concerns are not accepted"
  printf '| # | Verdict | Resolution |\n|---|---|---|\n| C1 w | AGREE | task A |\n| C2 x | PARTLY | task B |\n' > "$d/impl/explorer-deletion-review-response.md"
  run "CONCERNS with a closed response passes" 0 "^ok: .*CONCERNS accepted: 2 concern rows"
  printf '| C3 y | AGREE | OPEN |\n' >> "$d/impl/explorer-deletion-review-response.md"
  run "CONCERNS with an OPEN row fails" 1 "has open concerns"
  rm "$d/impl/explorer-deletion-review-response.md"
  review BLOCK "$h"
  run "BLOCK fails" 1 "says VERDICT: BLOCK"
  review PASS "not-a-hash"
  run "a malformed hash line fails" 1 "line 2 is not 'reviewed-adr-006-sha256"
  printf 'PASS\n' > "$d/impl/explorer-deletion-architecture-review.md"
  run "a malformed verdict line fails" 1 "line 1 is not 'VERDICT"
  review PASS "$h"; printf 'VERDICT: BLOCK\nreviewed-adr-006-sha256: %s\nreviewed-commit: abc1234\n' "$h" > "$d/impl/explorer-deletion-architecture-review-2.md"
  run "the newest review (-2) wins over the first" 1 "explorer-deletion-architecture-review-2.md says VERDICT: BLOCK"
  rm "$d/impl/explorer-deletion-architecture-review-2.md"
  # recorded plan hashes
  printf '#### Story 6.6.1: rules\nM1 a\n#### Story 6.6.2: next\n- The order of the apply\n  step one\n- Other\n' > "$d/impl/plan.md"
  local rt ao
  rt=$(printf '#### Story 6.6.1: rules\nM1 a\n' | sha256sum | cut -d' ' -f1); ao=$(printf -- '- The order of the apply\n  step one\n' | sha256sum | cut -d' ' -f1)
  printf 'VERDICT: PASS\nreviewed-adr-006-sha256: %s\nreviewed-commit: abc1234\nreviewed-rule-table-sha256: %s\nreviewed-apply-order-sha256: %s\n' "$h" "$rt" "$ao" > "$d/impl/explorer-deletion-architecture-review.md"
  run "matching rule-table and apply-order hashes pass" 0 "^ok: .*PASS covers"
  sed -i 's/M1 a/M1 b/' "$d/impl/plan.md"
  run "a changed rule table is STALE" 1 "different rule-table text"
  sed -i 's/M1 b/M1 a/; s/step one/step two/' "$d/impl/plan.md"
  run "a changed apply order is STALE" 1 "different apply-order text"
  # planning directory absent (hosted CI)
  run "absent planning dir skips by default" 0 "^SKIP: planning directory" DC_ADR_DIR="$d/absent"
  run "absent planning dir fails with DC_REQUIRE_ADR_REVIEW=1" 1 "FAIL: planning directory .* not found" DC_ADR_DIR="$d/absent" DC_REQUIRE_ADR_REVIEW=1
  echo "selftest: $n ok, $bad bad"; [ $bad = 0 ]
}

if [ "${1:-}" = --selftest ]; then selftest; else check; fi

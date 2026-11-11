#!/usr/bin/env bash
# check-case-table.sh [repo-root] | --selftest
# Each adversarial suite carries a header table between CASE-TABLE-BEGIN and CASE-TABLE-END, one row per case:
#   //! <ID> | <invariant(s)> | <test name>
# The table must match the #[test] functions of the file one to one: no row without a test, no test without a row,
# no duplicate id or name, no empty invariant column. A #[ignore] or #[should_panic] test fails the check (an
# ignored test is never run, and should_panic turns a failing case into a passing one).
# Scope: every .rs file in ci/delete_adversarial.sha256. The two adversarial suites (TABLE_REQUIRED) must carry a table;
# the mutation-killing and fd-limit suites need none, but the #[ignore]/#[should_panic] rule holds for all of them.
set -u
PIN=ci/delete_adversarial.sha256
TABLE_REQUIRED="crates/adapters/tests/delete_adversarial.rs crates/core/tests/floor_rule.rs"

check_file() {
  local f=$1 rc=0 rows tests ids names dup
  [ -f "$f" ] || { echo "FAIL: $f missing"; return 1; }
  rows=$(sed -n '/CASE-TABLE-BEGIN/,/CASE-TABLE-END/p' "$f" | grep -E '^//! *[A-Z]+-[0-9]+ *\|' | sed -E 's|^//! *||')
  if [ -z "$rows" ]; then
    case " $TABLE_REQUIRED " in *" $f "*) echo "FAIL: $f has no case table"; return 1 ;; esac
    if grep -nE '#\[(ignore|should_panic)' "$f" >/dev/null; then
      echo "FAIL: $f contains #[ignore] or #[should_panic]: $(grep -nE '#\[(ignore|should_panic)' "$f" | head -3 | tr '\n' ' ')"; return 1
    fi
    echo "ok: $f has no case table (not required); no #[ignore] or #[should_panic]"; return 0
  fi
  ids=$(echo "$rows" | cut -d'|' -f1 | tr -d ' ')
  names=$(echo "$rows" | cut -d'|' -f3 | tr -d ' ')
  tests=$(awk '/^[[:space:]]*#\[test\]/{t=1; next} t && /^[[:space:]]*(pub )?fn /{ sub(/^[[:space:]]*(pub )?fn /,""); sub(/\(.*/,""); print; t=0 } t && !/^[[:space:]]*#\[/ {t=0}' "$f")
  dup=$(echo "$ids" | sort | uniq -d); [ -z "$dup" ] || { echo "FAIL: $f duplicate case ids: $dup"; rc=1; }
  dup=$(echo "$names" | sort | uniq -d); [ -z "$dup" ] || { echo "FAIL: $f duplicate case names: $dup"; rc=1; }
  while IFS='|' read -r id inv name; do
    inv=$(echo "$inv" | tr -d ' ')
    [ -n "$inv" ] || { echo "FAIL: $f row $(echo "$id" | tr -d ' ') has an empty invariant column"; rc=1; }
  done <<< "$rows"
  local missing extra
  missing=$(comm -23 <(echo "$names" | sort) <(echo "$tests" | sort))
  extra=$(comm -13 <(echo "$names" | sort) <(echo "$tests" | sort))
  [ -z "$missing" ] || { echo "FAIL: $f table rows without a #[test]:"; echo "$missing" | sed 's/^/  /'; rc=1; }
  [ -z "$extra" ] || { echo "FAIL: $f #[test] functions without a table row:"; echo "$extra" | sed 's/^/  /'; rc=1; }
  if grep -nE '#\[(ignore|should_panic)' "$f" >/dev/null; then
    echo "FAIL: $f contains #[ignore] or #[should_panic]: $(grep -nE '#\[(ignore|should_panic)' "$f" | head -3 | tr '\n' ' ')"; rc=1
  fi
  [ $rc = 0 ] && echo "ok: $f $(echo "$names" | wc -l) cases match the table"
  return $rc
}

check() {
  local root=$1 rc=0 f FILES
  cd "$root" || { echo "FAIL: no such repo $root"; return 2; }
  [ -f "$PIN" ] || { echo "FAIL: $PIN missing"; return 2; }
  FILES="$TABLE_REQUIRED $(grep -oE '[^ ]+\.rs$' "$PIN" | grep -vxF -e "${TABLE_REQUIRED%% *}" -e "${TABLE_REQUIRED##* }" | tr '\n' ' ')"
  for f in $FILES; do check_file "$f" || rc=1; done
  return $rc
}

selftest() {
  local self bad=0 n=0
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "$d"' EXIT INT TERM
  expect() { local want=$1 name=$2 pat=$3; shift 3; local out rc; out=$("$@" 2>&1); rc=$?
    if { [ "$want" = pass ] && [ $rc = 0 ]; } || { [ "$want" = fail ] && [ $rc = 1 ]; }; then
      if echo "$out" | grep -q -- "$pat"; then n=$((n+1)); echo "ok: $name"; else echo "SELFTEST BAD: $name (output lacks '$pat'): $out"; bad=$((bad+1)); fi
    else echo "SELFTEST BAD: $name (rc=$rc): $out"; bad=$((bad+1)); fi; }
  mk() {  # $1 = adapters-file body, $2 = core-file body
    rm -rf "$d/r"; mkdir -p "$d/r/crates/adapters/tests" "$d/r/crates/core/tests" "$d/r/ci"
    printf '%s  %s\n' x crates/adapters/tests/delete_adversarial.rs x crates/core/tests/floor_rule.rs > "$d/r/ci/delete_adversarial.sha256"
    printf '%s\n' "$1" > "$d/r/crates/adapters/tests/delete_adversarial.rs"
    printf '%s\n' "$2" > "$d/r/crates/core/tests/floor_rule.rs"; }
  local good_a good_c
  good_a='//! CASE-TABLE-BEGIN
//! A-01 | I1 | one_case
//! A-02 | I2,I3 | two_case
//! CASE-TABLE-END
#[test]
fn one_case() {}
#[test]
fn two_case() {}
fn helper() {}'
  good_c='//! CASE-TABLE-BEGIN
//! F-01 | allowed | only_case
//! CASE-TABLE-END
#[test]
fn only_case() {}'
  mk "$good_a" "$good_c"
  expect pass "matching tables" "2 cases match" "$self" "$d/r"
  mk "${good_a/A-02 | I2,I3 | two_case/A-02 | I2,I3 | renamed_case}" "$good_c"
  expect fail "row whose test does not exist" "table rows without a #\[test\]" "$self" "$d/r"
  mk "$good_a
#[test]
fn smuggled_in() {}" "$good_c"
  expect fail "test without a row" "without a table row" "$self" "$d/r"
  mk "${good_a/A-02 | I2,I3 | two_case/A-02 |  | two_case}" "$good_c"
  expect fail "empty invariant column" "empty invariant" "$self" "$d/r"
  mk "${good_a/A-02 | I2,I3 | two_case/A-01 | I2 | two_case}" "$good_c"
  expect fail "duplicate id" "duplicate case ids" "$self" "$d/r"
  mk "${good_a/fn two_case() {\}/#[ignore]
fn two_case() {\}}" "$good_c"
  expect fail "ignored test" "#\[ignore\]" "$self" "$d/r"
  mk "${good_a/fn two_case() {\}/#[should_panic]
fn two_case() {\}}" "$good_c"
  expect fail "should_panic test" "should_panic" "$self" "$d/r"
  mk "fn x() {}" "$good_c"
  expect fail "file without a table" "no case table" "$self" "$d/r"
  mk "$good_a" "${good_c/only_case/other_case}
#[test]
fn only_case() {}"
  expect fail "second file mismatched" "floor_rule.rs" "$self" "$d/r"
  mk "$good_a" "$good_c"
  printf '#[test]\nfn t() {}\n' > "$d/r/crates/adapters/tests/delete_aa_mutants.rs"; echo "x  crates/adapters/tests/delete_aa_mutants.rs" >> "$d/r/ci/delete_adversarial.sha256"
  expect pass "pinned file without a table needs none" "no case table (not required)" "$self" "$d/r"
  printf '#[test]\n#[ignore]\nfn t() {}\n' > "$d/r/crates/adapters/tests/delete_aa_mutants.rs"
  expect fail "ignored test in a table-less pinned file" "#\[ignore\]" "$self" "$d/r"
  echo "selftest: $n ok, $bad bad"; [ $bad = 0 ]
}

case "${1:-.}" in --selftest) selftest ;; *) check "${1:-.}" ;; esac

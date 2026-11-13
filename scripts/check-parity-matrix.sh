#!/usr/bin/env bash
# check-parity-matrix.sh <validation.md> [--wrapper-tests DIR] | --selftest
# Story 5.2.1: every row `| P-NN | old | new | test rows |` of the parity matrix must name a test that exists or
# retire the old behaviour explicitly (the "new behaviour" cell says "retired"). Exit 0 when every row does, 1 naming
# each row that does neither, 2 on a usage error.
# A test id (T-nnn, S-nn) resolves through the test table of the same file (`| ID | req | story | file | `name` |`).
# The name must exist as `fn <name>` under crates/ or as literal text in a file under --wrapper-tests (the dotfiles
# stapler-scripts/tests tags each wrapper check with its S-nn name). This checks existence, not a green run:
# `cargo test --workspace` and the wrapper_*.sh scripts are the runs. A row needs one resolving id, not all of them.
set -u

# Test ids in a cell; `T-080..T-092` expands to every id in the range.
expand_ids() {
  local cell=$1 a b n
  echo "$cell" | grep -oE '\b[TS]-[0-9]+\b'
  echo "$cell" | grep -oE '[TS]-[0-9]+\.\.[TS]-[0-9]+' | while IFS= read -r r; do
    a=${r%%..*}; b=${r##*..}
    for ((n = 10#${a#?-}; n <= 10#${b#?-}; n++)); do printf '%s-%03d\n' "${a%%-*}" "$n"; done
  done
}

check() {
  local matrix=$1 tests_dir=$2 root=$3 rc=0 rows row id cell new name found
  rows=$(grep -E '^\| P-[0-9]+ \|' "$matrix") || { echo "FAIL: no P-nn rows in $matrix"; return 1; }
  while IFS= read -r row; do
    id=$(echo "$row" | cut -d'|' -f2 | tr -d ' ')
    new=$(echo "$row" | cut -d'|' -f4)
    cell=$(echo "$row" | cut -d'|' -f5)
    if echo "$new" | grep -qiE '\bretire[sd]?\b|\bretirement\b'; then echo "ok:   $id retired"; continue; fi
    found=""
    for tid in $(expand_ids "$cell"); do
      name=$(grep -E "^\| $tid \|" "$matrix" | head -1 | cut -d'|' -f6 | tr -d ' `')
      [ -n "$name" ] || continue
      if grep -rqE "fn $name\b" "$root/crates" 2>/dev/null \
        || { [ -n "$tests_dir" ] && grep -rqF "$name" "$tests_dir" 2>/dev/null; }; then found="$tid"; break; fi
    done
    if [ -n "$found" ]; then echo "ok:   $id via $found"
    else echo "FAIL: $id names no existing test (cell: $(echo "$cell" | tr -s ' ')) and is not retired"; rc=1; fi
  done <<<"$rows"
  return $rc
}

selftest() {
  local d rc=0; d=$(mktemp -d "${TMPDIR:-/tmp}/parity.XXXXXX"); trap 'rm -rf "$d"' RETURN
  mkdir -p "$d/crates/a/tests" "$d/wt"
  printf '#[test]\nfn real_case_should_pass_When_x() {}\n' >"$d/crates/a/tests/t.rs"
  echo '# tag S-02: wrapper_case_should_pass_When_y' >"$d/wt/w.sh"
  cat >"$d/m.md" <<'M'
| P-01 | old | new | T-001 |
| P-02 | old | retired in v1 | |
| P-03 | old | new | T-009, S-02 |
| T-001 | R | 1 | `a/tests/t.rs` | `real_case_should_pass_When_x` | Unit |
| T-009 | R | 1 | `a/tests/t.rs` | `missing_case_should_pass_When_z` | Unit |
| S-02 | R | 1 | `w.sh` | `wrapper_case_should_pass_When_y` | Integration |
M
  check "$d/m.md" "$d/wt" "$d" >/dev/null || { echo "selftest: good matrix rejected"; rc=1; }
  sed -i 's/^| P-03 | old | new | T-009, S-02 |/| P-03 | old | new | T-009 |/' "$d/m.md"
  out=$(check "$d/m.md" "$d/wt" "$d") && { echo "selftest: uncovered row accepted"; rc=1; }
  echo "$out" | grep -q 'FAIL: P-03' || { echo "selftest: P-03 not named"; rc=1; }
  sed -i 's/^| P-03 .*$/| P-03 | old | new | |/' "$d/m.md"
  check "$d/m.md" "$d/wt" "$d" >/dev/null && { echo "selftest: empty test cell accepted"; rc=1; }
  [ $rc -eq 0 ] && echo "selftest ok"
  return $rc
}

root=$(cd "$(dirname "$0")/.." && pwd)
case "${1:-}" in
  --selftest) selftest; exit $? ;;
  "" | -*) echo "usage: $0 <validation.md> [--wrapper-tests DIR] | --selftest" >&2; exit 2 ;;
esac
matrix=$1; tests_dir=""
[ "${2:-}" = "--wrapper-tests" ] && tests_dir=${3:-}
[ -f "$matrix" ] || { echo "$matrix: not found" >&2; exit 2; }
check "$matrix" "$tests_dir" "$root"

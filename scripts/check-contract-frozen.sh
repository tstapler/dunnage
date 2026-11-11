#!/usr/bin/env bash
# check-contract-frozen.sh [--base REV] [repo-root] | --update [repo-root] | --selftest
# The delete-engine contract (Story 0.1.3, C7) is the fenced `contract` block in crates/adapters/src/delete.rs
# plus the listed items below. Their normalised text must equal ci/contract.frozen.txt, and a change to
# that file needs a commit in BASE..HEAD whose message contains `contract-change:` (BASE defaults to HEAD~1;
# when the frozen file does not exist at BASE this is the initial freeze and is accepted).
set -uo pipefail

DELETE_RS=crates/adapters/src/delete.rs
FROZEN=ci/contract.frozen.txt
# kind|file|regex   B = item through the closing brace at column 0 (with its attributes), S = signature only
ITEMS='B|crates/core/src/ids.rs|^pub struct (Identity|RootDev|AllocatedBytes|ApparentBytes)([^A-Za-z_]|$)
S|crates/core/src/ids.rs|^    pub fn (of|on_dev)[(]
B|crates/core/src/delete_types.rs|^pub (struct|enum) (Freed|DeleteError|DeleteFailure|DeletePolicy|AllowedTarget|NamePattern|StatInfo|FileKind|DirEntryInfo|AbortFlag)([^A-Za-z_]|$)
S|crates/core/src/delete_types.rs|^    pub fn (new|set|is_set)[(]
S|crates/core/src/delete_types.rs|^pub (const CMD_MSG_MAX|fn first_line_capped)
S|crates/core/src/floor_rule.rs|^    pub fn (validate|for_allowed)[(]
B|crates/core/src/allowed.rs|^pub struct ExplorerScope([^A-Za-z_]|$)
B|crates/core/src/apply_token.rs|^pub struct ApplyToken
S|crates/core/src/apply_token.rs|^    pub fn (grant|for_tests)[(]
S|crates/adapters/src/dirops.rs|^    pub fn (confined_to|unconfined)[(]
B|crates/adapters/src/dirops.rs|^pub (struct (VerifiedRoot|VerifiedParent)|trait DirOps)([^A-Za-z_]|$)
S|crates/adapters/src/dirops.rs|^pub (fn (open_verified_root|open_parent)|const DELETE_BATCH)
S|crates/adapters/src/delete.rs|^pub fn delete_tree'

extract_item() {  # kind file regex  (reads file in $PWD); prints every match, normalised
  local kind=$1 file=$2 re=$3
  [ -f "$file" ] || return 0
  kind="$kind" pat="$re" awk 'BEGIN { kind = ENVIRON["kind"]; pat = ENVIRON["pat"] }
    function emit(s) { sub(/^[ \t]+/, "", s); print s }
    /^[ \t]*#\[/ { attrs = attrs $0 "\n"; next }
    /^[ \t]*\/\/\// { next }
    open { print_line($0); if (kind == "B" && $0 ~ /^}/) open = 0; else if (kind == "S" && ($0 ~ /\{[ \t]*$/ || $0 ~ /;[ \t]*$/ || $0 ~ /\{.*\}[ \t]*$/)) open = 0; next }
    $0 ~ pat { n = split(attrs, a, "\n"); for (i = 1; i <= n; i++) if (a[i] != "") { if (kind == "S") emit(a[i]); else print a[i] }
               attrs = ""; print_line($0)
               if (kind == "B" && ($0 ~ /^}/ || $0 ~ /;[ \t]*$/)) next
               if (kind == "S" && ($0 ~ /\{[ \t]*$/ || $0 ~ /;[ \t]*$/ || $0 ~ /\{.*\}[ \t]*$/)) next
               open = 1; next }
    { attrs = "" }
    function print_line(s) { if (kind == "S") emit(s); else print s }
  ' "$file"
}

contract_block() {  # doc lines of the fenced contract block(s), `//! ` stripped
  [ -f "$DELETE_RS" ] || return 0
  awk '/^\/\/! ```contract/ { on = 1; next } on && /^\/\/! ```/ { on = 0; next }
       on { sub(/^\/\/! ?/, ""); print }' "$DELETE_RS"
}

render() {  # prints the normalised contract text; returns 1 when any section is empty
  local bad=0 block out kind file re
  block=$(contract_block)
  if [ -z "$block" ]; then echo "FAIL: no \`contract\` block in $DELETE_RS" >&2; bad=1; fi
  echo "## contract block ($DELETE_RS)"; printf '%s\n' "$block"
  while IFS='|' read -r kind file re; do
    out=$(extract_item "$kind" "$file" "$re")
    if [ -z "$out" ]; then echo "FAIL: item not found: $file /$re/" >&2; bad=1; fi
    echo "## $file /$re/"; printf '%s\n' "$out"
  done <<<"$ITEMS"
  return "$bad"
}

check() {
  local root=$1 base=$2 cur rc=0 bsha
  cd "$root" || { echo "FAIL: no repo $root"; return 2; }
  cur=$(render) || return 1
  dfile=$(mktemp)
  [ -f "$FROZEN" ] || { echo "FAIL: $FROZEN missing (run scripts/check-contract-frozen.sh --update)"; return 1; }
  if ! diff -u "$FROZEN" <(printf '%s\n' "$cur") >"$dfile" 2>&1; then
    echo "FAIL: contract text differs from $FROZEN:"; head -40 "$dfile"; rm -f "$dfile"; return 1
  fi
  rm -f "$dfile"
  if [ -z "$base" ]; then base=HEAD~1; fi
  bsha=$(git rev-parse --verify -q "$base^{commit}") || { echo "ok: contract equals $FROZEN (no base commit to compare)"; return 0; }
  # An initial freeze is only the first appearance: a frozen file with any history at BASE (deleted, then re-added) is a change.
  if ! git cat-file -e "$bsha:$FROZEN" 2>/dev/null && [ -z "$(git log --format=%H -1 "$bsha" -- "$FROZEN")" ]; then
    echo "ok: initial freeze of the contract (no $FROZEN, and no history of it, at $base)"; return 0
  fi
  if git diff --quiet "$bsha" -- "$FROZEN"; then echo "ok: contract equals $FROZEN and it is unchanged since $base"; return 0; fi
  # The marker must sit on a commit that itself changes the frozen file, not on any commit in the range.
  if [ -n "$(git log --format=%H -1 --grep='contract-change:' "$bsha..HEAD" -- "$FROZEN")" ]; then
    echo "ok: contract changed since $base with a contract-change: commit (re-run the independent test author)"; return 0
  fi
  echo "FAIL: $FROZEN changed since $base but no commit in $base..HEAD that changes it carries 'contract-change:'"; return 1
}

selftest() {
  local self bad=0 n=0 out rc
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "${d:-}"' EXIT INT TERM
  local src; src=$(cd "$(dirname "$self")/.." && pwd)
  expect() { local want=$1 name=$2 pat=$3; shift 3; out=$("$@" 2>&1); rc=$?
    if [ "$rc" = "$want" ] && grep -q -- "$pat" <<<"$out"; then n=$((n+1)); echo "ok: $name"; else echo "SELFTEST BAD: $name (rc=$rc, want $want): $out"; bad=$((bad+1)); fi; }
  mkdir -p "$d/r"; (cd "$src" && tar cf - crates/*/src) | tar xf - -C "$d/r"; cd "$d/r" || exit 2
  git init -q -b main; git config user.email t@t; git config user.name t
  mkdir ci; "$self" --update . >/dev/null; git add -A; git commit -qm "freeze"
  expect 0 "the initial freeze is accepted (no base to compare)" "^ok" "$self" .
  echo "// touch" >> crates/core/src/ids.rs; expect 0 "a body-only edit outside the listed items passes" "^ok" "$self" .; git checkout -q crates
  sed -i 's/pub fn open_parent</pub fn open_parent_x</' crates/adapters/src/dirops.rs
  expect 1 "a renamed signature fails" "differs from ci/contract.frozen.txt" "$self" .; git checkout -q crates
  sed -i 's|^//! *pub const CMD_MSG_MAX: usize = 200;|//! pub const CMD_MSG_MAX: usize = 201;|' crates/adapters/src/delete.rs
  expect 1 "an edit inside the contract block fails" "differs from" "$self" .; git checkout -q crates
  sed -i 's/pub restore_owner_write: bool,/pub restore_owner_write: bool,\n    pub extra: u8,/' crates/core/src/delete_types.rs
  expect 1 "an added DeletePolicy field fails" "differs from" "$self" .; git checkout -q crates
  sed -i 's/^pub struct ApplyToken(());/#[derive(Clone)]\npub struct ApplyToken(());/' crates/core/src/apply_token.rs
  expect 1 "a derive added to ApplyToken fails" "differs from" "$self" .; git checkout -q crates
  sed -i 's/pub fn open_parent</pub fn open_parent_x</' crates/adapters/src/dirops.rs; "$self" --update . >/dev/null 2>&1; git add -A; git commit -qm "refactor: rename"
  expect 1 "an updated frozen file without a contract-change: commit fails" "contract-change:" "$self" .
  git commit -q --allow-empty -m "x" ; sed -i 's/open_parent_x/open_parent_y/' crates/adapters/src/dirops.rs; "$self" --update . >/dev/null 2>&1; git add -A
  git commit -qm "contract-change: rename open_parent again (test author re-run)"
  expect 0 "a contract-change: commit passes" "contract-change:" "$self" .
  git reset -q --hard HEAD~1; git commit -q --allow-empty -m "contract-change: marker on an empty commit"
  sed -i 's/open_parent_x/open_parent_y/' crates/adapters/src/dirops.rs; "$self" --update . >/dev/null 2>&1; git add -A; git commit -qm "refactor: change without the marker"
  expect 1 "a contract-change: marker on a commit that does not change the frozen file fails" "that changes it" "$self" --base HEAD~2 .
  git reset -q --hard HEAD~2; git commit -q --allow-empty -m "x"; git rm -q ci/contract.frozen.txt; git commit -qm "chore: drop the frozen file"
  "$self" --update . >/dev/null 2>&1; git add -A; git commit -qm "chore: bring it back"
  expect 1 "a re-added frozen file with history is not an initial freeze" "that changes it" "$self" --base HEAD~1 .
  rm ci/contract.frozen.txt; expect 1 "a missing frozen file fails" "missing" "$self" .; git checkout -q ci
  rm crates/adapters/src/delete.rs; expect 1 "a missing contract block fails closed" "no .contract. block" "$self" .; git checkout -q crates
  sed -i 's/pub fn delete_tree/pub fn delete_tree_renamed/' crates/adapters/src/delete.rs
  expect 1 "a renamed delete_tree fails (item not found or differs)" "FAIL" "$self" .
  echo "selftest: $n ok, $bad bad"; [ $bad = 0 ]
}

case "${1:-}" in
  --selftest) selftest ;;
  --update) root=${2:-.}; cd "$root" || exit 2; mkdir -p ci; if render > "$FROZEN.new"; then mv "$FROZEN.new" "$FROZEN"; echo "wrote $FROZEN"; else rm -f "$FROZEN.new"; exit 1; fi ;;
  --base) check "${3:-.}" "${2:?rev}" ;;
  *) check "${1:-.}" "" ;;
esac

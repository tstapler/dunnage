#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2016,SC2295
# gate.sh <A|B|C|D|E|F> [repo-root] | --selftest
# Local stand-in for CI: runs every check the gate requires, on a clean tree, and writes a report bound to the commit.
# ci/gate.d/*.required (one file per owning story, so stories never edit one shared file) hold lines
# "<first-gate> <last-gate|-> <command...>" ; a command whose script file is missing is a FAIL, so a gate
# cannot go green by omitting a check. Every scripts/check-*.sh not listed is run too (and its --selftest if it has one).
set -u
order="ABCDEF"
rank() { local p=${order%%$1*}; echo ${#p}; }

run_gate() {
  local gate=$1 root=${2:-.} fails=0 ok=0 sha line cmd first last script prev
  cd "$root" || { echo "FAIL: no repo $root"; return 2; }
  case $gate in A|B|C|D|E|F) ;; *) echo "usage: gate.sh A|B|C|D|E|F"; return 64 ;; esac
  ls ci/gate.d/*.required >/dev/null 2>&1 || { echo "FAIL: ci/gate.d/*.required missing"; return 1; }
  [ -z "$(git status --porcelain)" ] || { echo "FAIL: working tree is not clean; the report must name a commit"; return 1; }
  sha=$(git rev-parse --short HEAD)
  # The bench scripts build the gate's binary with Bazel instead of Cargo (see scripts/gate-binary.sh).
  [ "${DC_GATE_SKIP_BUILD:-${DC_GATE_SKIP_CARGO:-0}}" = 1 ] || export DC_BENCH_BAZEL=1
  export DC_REQUIRE_ADR_REVIEW=${DC_REQUIRE_ADR_REVIEW:-1}   # locally the ADR-006 review check must find the planning dir (Task 6.6.2e); hosted CI leaves it unset
  step() { local name=$1; shift; if "$@" >/dev/null 2>&1; then ok=$((ok+1)); echo "ok:   $name"; else fails=$((fails+1)); echo "FAIL: $name"; fi; }
  while read -r first last cmd; do
    [ -n "${first:-}" ] && [ "${first:0:1}" != "#" ] || continue
    [ "$(rank "$first")" -le "$(rank "$gate")" ] || continue
    [ "$last" = - ] || [ "$(rank "$gate")" -le "$(rank "$last")" ] || continue
    script=${cmd%% *}
    if [ ! -e "$script" ] && ! command -v "$script" >/dev/null 2>&1; then fails=$((fails+1)); echo "FAIL: required check missing: $script (needed from gate $first)"; continue; fi
    step "$cmd" bash -c "$cmd"
  done < <(cat ci/gate.d/*.required)
  for script in scripts/check-*.sh; do
    [ -e "$script" ] || continue
    cat ci/gate.d/*.required | grep -qE "^[A-F] [A-F-] $script( |$)" || step "$script" "$script"
    if grep -q -- '--selftest' "$script"; then step "$script --selftest" "$script" --selftest; fi
  done
  # Build, lint and test with Bazel (ADR-008); the Cargo leg keeps the tests Bazel cannot run hermetically
  # (they build release profiles with cargo) and a release build that detects Cargo/Bazel drift.
  # DC_GATE_SKIP_BUILD=1 (alias DC_GATE_SKIP_CARGO) skips this block, for the selftest.
  local bin=${DC_GATE_BINARY:-} copy=""
  if [ "${DC_GATE_SKIP_BUILD:-${DC_GATE_SKIP_CARGO:-0}}" != 1 ]; then
    # The kinds a gate registers are a build setting (Story 2.3.4): gate B and later build with the gate's feature,
    # so the binary the gate stamps is the one whose registry matches ci/safety/kinds-by-gate.txt. Gate A has none.
    local cfg=() feat=()
    case $gate in B) cfg=(--config=gate-b); feat=(--features dunnage/gate-b) ;; C) cfg=(--config=gate-c); feat=(--features dunnage/gate-c) ;;
      D|E|F) cfg=(--config=gate-d); feat=(--features dunnage/gate-d) ;; esac
    step "bazel build //..." scripts/bazel.sh build //... "${cfg[@]}"
    step "bazel lint (rustfmt + clippy -D warnings)" scripts/bazel.sh build --config=lint //... "${cfg[@]}"
    step "bazel test //..." scripts/bazel.sh test //... "${cfg[@]}" --test_env=GATE="$gate"
    step "cargo-only tests (release profile builds)" env GATE="$gate" cargo test -p dunnage "${feat[@]}" --test kind_registry --test e2e_apply_caches --test explore_hostile
    step "cargo build --release (Cargo/Bazel drift leg)" cargo build --release --locked --workspace "${feat[@]}"
    if [ -z "$bin" ] && [ "$fails" = 0 ]; then
      if bin=$(scripts/gate-binary.sh "$gate" 2>/dev/null); then ok=$((ok+1)); echo "ok:   bazel -c opt gate binary $bin"; else fails=$((fails+1)); echo "FAIL: bazel -c opt gate binary"; fi
    fi
  fi
  # The binary the gate stamps (and the owner installs) carries no test-support marker and none of the explorer's
  # test-only switches (Story 6.5.6).
  if [ -n "$bin" ] && [ -f "$bin" ] && [ -e scripts/check-release-features.sh ]; then step "gated binary: no test-support marker or switch" scripts/check-release-features.sh --artifact "$bin"; fi
  prev=$(ls -t "$(git rev-parse --git-dir)/gate-reports/$gate-"*.txt 2>/dev/null | head -1)
  prev=${prev##*/}; prev=${prev#"$gate"-}; prev=${prev%.txt}
  echo "pin diff since previous gate $gate report (${prev:-none}):"
  if [ -n "$prev" ] && git rev-parse -q --verify "$prev^{commit}" >/dev/null; then git diff "$prev" HEAD -- ci/delete_adversarial.sha256 | sed 's/^/  /'; else echo "  (no previous report to compare with)"; fi
  line="GATE $gate commit $sha tree clean checks $((ok+fails)) ok $ok failed $fails"
  mkdir -p "$(git rev-parse --git-dir)/gate-reports"; echo "$line" > "$(git rev-parse --git-dir)/gate-reports/$gate-$sha.txt"; echo "$line"
  # Gate stamp (Task 0.1.1d, consumed by Task 2.2.1d): only an all-green gate on a clean tree stamps the release binary it built.
  rm -f "$(git rev-parse --git-dir)/gate-reports/stamp-$gate.json"
  if [ $fails = 0 ] && [ -n "$bin" ] && [ -f "$bin" ]; then
    # Bazel reuses one output path for every gate: keep the exact stamped file for `make install-gated`.
    copy=$(git rev-parse --git-dir)/gate-reports/bin-$gate; cp -f "$bin" "$copy"
    printf '{"schema":1,"gate":"%s","commit":"%s","binary_sha256":"%s"}\n' "$gate" "$(git rev-parse HEAD)" "$(sha256sum "$bin" | cut -d' ' -f1)" \
      > "$(git rev-parse --git-dir)/gate-reports/stamp-$gate.json"
    echo "stamp: $(git rev-parse --git-dir)/gate-reports/stamp-$gate.json"
    # The owner cuts the tag after OWNER WAIT F (RELEASE.md); the gate never creates one.
    [ "$gate" != F ] || echo "gate F is green at $(git rev-parse --short HEAD). After OWNER WAIT F (RELEASE.md) the owner tags it: git tag explore-gate-f $(git rev-parse HEAD)"
  fi
  [ $fails = 0 ]
}

selftest() {
  local self bad=0 n=0 out rc
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "$d"' EXIT INT TERM
  export DC_GATE_SKIP_CARGO=1
  expect() { local want=$1 name=$2 pat=$3; shift 3; out=$("$@" 2>&1); rc=$?
    if { [ "$want" = pass ] && [ $rc = 0 ]; } || { [ "$want" = fail ] && [ $rc = 1 ]; } || { [ "$want" = usage ] && [ $rc = 64 ]; }; then
      if echo "$out" | grep -q -- "$pat"; then n=$((n+1)); echo "ok: $name"; else echo "SELFTEST BAD: $name (output lacks '$pat'): $out"; bad=$((bad+1)); fi
    else echo "SELFTEST BAD: $name (rc=$rc): $out"; bad=$((bad+1)); fi; }
  mkdir -p "$d/r/scripts" "$d/r/ci/gate.d"; cd "$d/r" || exit 2; git init -q -b main; git config user.email t@t; git config user.name t
  printf '#!/bin/sh\nexit 0\n' > scripts/check-good.sh; chmod +x scripts/check-good.sh
  printf '#!/bin/sh\n# supports --selftest\necho "$*" >> "$GATE_ARGS_LOG"\nexit 0\n' > scripts/check-token-sites.sh; chmod +x scripts/check-token-sites.sh
  printf 'A - scripts/check-good.sh\nA A scripts/check-token-sites.sh --expect-sites 0\nB - scripts/check-token-sites.sh --expect-sites 1\nC - scripts/check-worktree.sh\n' > ci/gate.d/base.required
  git add -A; git commit -qm init
  expect pass "gate A runs only what is required from A" "checks 3 ok 3 failed 0" "$self" A "$d/r"
  export GATE_ARGS_LOG="$d/args.log"; : > "$GATE_ARGS_LOG"
  expect pass "gate B adds the token-site check and its selftest" "GATE B commit .* checks 3 ok 3 failed 0" "$self" B "$d/r"
  if grep -qx -- '--expect-sites 1' "$GATE_ARGS_LOG" && ! grep -qx -- '--expect-sites 0' "$GATE_ARGS_LOG"; then n=$((n+1)); echo "ok: at gate B only the 'B -' argument form ran, not the 'A A' one"; else echo "SELFTEST BAD: argument forms at gate B: $(tr '\n' ',' < "$GATE_ARGS_LOG")"; bad=$((bad+1)); fi
  expect fail "gate F is accepted, not a usage error, and ranks after C" "required check missing: scripts/check-worktree.sh (needed from gate C)" "$self" F "$d/r"
  expect fail "gate C fails when a required script does not exist yet" "required check missing: scripts/check-worktree.sh" "$self" C "$d/r"
  printf '#!/bin/sh\nexit 1\n' > scripts/check-bad.sh; chmod +x scripts/check-bad.sh; git add -A; git commit -qm bad
  expect fail "a failing unlisted check fails the gate" "FAIL: scripts/check-bad.sh" "$self" A "$d/r"
  git rm -q scripts/check-bad.sh; git commit -qm rm
  echo dirty > scripts/untracked.txt
  expect fail "a dirty tree is refused" "not clean" "$self" A "$d/r"
  rm scripts/untracked.txt
  expect usage "an unknown gate letter is a usage error" "usage" "$self" Z "$d/r"
  ls .git/gate-reports/A-*.txt >/dev/null 2>&1 && { n=$((n+1)); echo "ok: a report file named after gate and commit was written"; } || { echo "SELFTEST BAD: no report file"; bad=$((bad+1)); }
  # stamp: written only when green and a binary exists; names the full commit and the binary hash; removed by a later failing run
  printf 'fake-binary-v1' > fakebin; export DC_GATE_BINARY="$d/r/fakebin"; git add -A; git commit -qm bin
  expect pass "a green gate with a binary prints the stamp path" "stamp: " "$self" A "$d/r"
  if [ "$(jq -r .binary_sha256 .git/gate-reports/stamp-A.json)" = "$(printf 'fake-binary-v1' | sha256sum | cut -d' ' -f1)" ] && [ "$(jq -r .commit .git/gate-reports/stamp-A.json)" = "$(git rev-parse HEAD)" ]; then n=$((n+1)); echo "ok: the stamp holds the binary sha256 and the full commit"; else echo "SELFTEST BAD: stamp content: $(cat .git/gate-reports/stamp-A.json 2>&1)"; bad=$((bad+1)); fi
  printf '#!/bin/sh\nexit 1\n' > scripts/check-bad.sh; chmod +x scripts/check-bad.sh; git add -A; git commit -qm bad2
  expect fail "a red gate fails" "FAIL: scripts/check-bad.sh" "$self" A "$d/r"
  [ ! -e .git/gate-reports/stamp-A.json ] && { n=$((n+1)); echo "ok: a red gate leaves no stamp (the old one is removed)"; } || { echo "SELFTEST BAD: stale stamp survived a red gate"; bad=$((bad+1)); }
  unset DC_GATE_BINARY
  echo "selftest: $n ok, $bad bad"; [ $bad = 0 ]
}
if [ "${1:-}" = --selftest ]; then selftest; else run_gate "${1:-}" "${2:-.}"; fi

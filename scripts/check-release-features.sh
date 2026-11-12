#!/usr/bin/env bash
# shellcheck disable=SC2154,SC2015
# Fails if a shipped disk-cleanup artifact can carry the `test-support` feature.
# Four independent legs (graph, built-feature set, marker bytes, marker reachability), each testable alone.
# Every leg fails closed: missing tool, missing profile, an unselected core crate or empty input is a FAIL.
#   check-release-features.sh                 Cargo leg (build -p disk-cleanup under release and dist) and Bazel leg
#                                             (bazel build --config=dist, the binary the release archives hold), all legs
#   check-release-features.sh --artifact BIN  run only the byte legs on a built artifact (release job)
#   check-release-features.sh --selftest      positive control + one negative per leg and per fail-open case (Cargo)
#   check-release-features.sh --bazel-selftest  the same controls for the Bazel leg, incl. a tainted Bazel build
# DC_RELEASE_FEATURES_LEGS=cargo|bazel|both (default both) picks the legs of the default run. The Cargo leg stays
# so that Cargo/Bazel drift (a feature set that differs between the two build systems) shows up as one leg failing.
set -euo pipefail
MARKER='dc-flavor=test-support;9f3ac7e1'   # == core::build_flavor() under test-support
PANIC_MARKER='DC_EXPLORE_PANIC_AT'          # the forced-panic switch of the Gate F journeys: test-support only (Story 6.5.6)
DELAY_MARKER='DC_EXPLORE_PLAN_DELAY_MS'     # the slow-planner switch of the full-disk journeys: test-support only
DELETE_MARKER='DC_EXPLORE_DELETE_DELAY_MS'  # the slow-delete switch of the live-delete journey: test-support only
FLAVOR_PREFIX='dc-flavor='                  # both flavors start with it; present iff main reaches build_flavor()
PKG=disk-cleanup CORE=disk-cleanup-core
CORE_TARGET=${CORE//-/_}                    # cargo prints lib target names with underscores
TAINT_DIR=""                                # set by selftest; removed by the trap on every exit path

die() { echo "FAIL: $*"; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "required tool '$1' not found on PATH (the check cannot run, so it does not pass)"; }

leg_resolver() { grep -Eq '^resolver *= *"(2|3)"' "${1:-Cargo.toml}"; }
# `inherits = "release"` must sit inside the [profile.dist] table, not in any other table.
leg_profile() {
  awk '/^\[/ { in_dist = ($0 ~ /^\[profile\.dist\][ \t]*(#.*)?$/); if (in_dist) seen = 1; next }
       in_dist && /^inherits[ \t]*=[ \t]*"release"[ \t]*(#.*)?$/ { ok = 1 }
       END { exit !(seen && ok) }' "${1:-Cargo.toml}"
}

# stdin: `cargo tree` text. Fails when the feature is on a normal/build edge, AND when the tree has no core
# node at all (a tree that never mentions the core crate proves nothing).
leg_graph() {
  local tree; tree=$(cat)
  printf '%s\n' "$tree" | grep -q "^$CORE v" || return 2
  ! printf '%s\n' "$tree" | grep -q "$CORE feature \"test-support\""
}

# stdin: cargo --message-format=json. Selects the core lib artifact by target name (not by package_id, whose
# `name@version` form cargo drops when the directory is named like the package). 0 = clean,
# 1 = test-support present, 2 = no core artifact seen or unparsable input (a failure, never a pass).
leg_features() {
  local out
  out=$(jq -s -r --arg t "$CORE_TARGET" '[.[] | select(.reason=="compiler-artifact" and .target.name==$t
        and (.target.kind|index("lib") or index("rlib")))]
        | if length == 0 then "none" elif any(.[]; .features|index("test-support")) then "taint" else "clean" end') || return 2
  case "$out" in clean) return 0 ;; taint) return 1 ;; *) return 2 ;; esac
}

# arg: file. Fails when the marker bytes are present.
leg_marker() { [ "$(grep -a -c -F -- "$MARKER" "$1")" -eq 0 ]; }
# arg: file. The test-support switches (forced panic, slow planner) must not be in a shipped artifact.
leg_panic_marker() { [ "$(grep -a -c -F -e "$PANIC_MARKER" -e "$DELAY_MARKER" -e "$DELETE_MARKER" -- "$1")" -eq 0 ]; }
# arg: file. A flavor string must be present: proves main reaches build_flavor(), so the marker would survive
# linking if the feature were on (the marker leg cannot be vacuous for this binary).
leg_reference() { [ "$(grep -a -c -F -- "$FLAVOR_PREFIX" "$1")" -ge 1 ]; }
# arg: file. Positive control: the scanner must see the marker in a known-tainted build.
scanner_sees_marker() { [ "$(grep -a -c -F -- "$MARKER" "$1")" -ge 1 ]; }
# The explorer's apply (and so its slow-delete switch) is compiled only into a gate-d build, so its control is separate.
scanner_sees_panic_marker() { grep -a -q -F -- "$PANIC_MARKER" "$1" && grep -a -q -F -- "$DELAY_MARKER" "$1"; }
scanner_sees_delete_marker() { grep -a -q -F -- "$DELETE_MARKER" "$1"; }

bin_from_json() { jq -r 'select(.reason=="compiler-artifact" and .executable != null
        and (.target.kind|index("bin")) and (.profile.test|not)) | .executable'; }

report_features() {  # $1 = profile label; stdin json
  local rc=0; leg_features || rc=$?
  case $rc in 0) return 0 ;; 1) echo "FAIL feature leg ($1): built $CORE has test-support"; return 1 ;;
    *) echo "FAIL feature leg ($1): no compiler-artifact for $CORE (target $CORE_TARGET) in cargo output, or unparsable output"; return 1 ;; esac
}

byte_legs() {  # $1 label, $2 file
  local rc=0
  leg_marker "$2" || { echo "FAIL marker leg ($1): marker bytes present in $2"; rc=1; }
  leg_panic_marker "$2" || { echo "FAIL marker leg ($1): $PANIC_MARKER, $DELAY_MARKER or $DELETE_MARKER (a test-support switch) is present in $2"; rc=1; }
  leg_reference "$2" || { echo "FAIL reference leg ($1): '$FLAVOR_PREFIX' absent from $2, so main does not reach build_flavor() and the marker leg is blind"; rc=1; }
  return "$rc"
}

build_and_check() {  # $1 = label, rest = cargo profile flags
  local label="$1"; shift
  local tree json bin rc=0 trc=0
  tree=$(cargo tree -p "$PKG" -e normal,build,features -i "$CORE" --target all) || die "cargo tree failed ($label)"
  printf '%s' "$tree" | leg_graph || trc=$?
  case $trc in 0) ;; 1) echo "FAIL graph leg ($label): $CORE feature test-support on a normal/build edge"; rc=1 ;;
    *) echo "FAIL graph leg ($label): cargo tree shows no $CORE node (is the crate named $CORE?)"; rc=1 ;; esac
  json=$(cargo build "$@" -p "$PKG" --message-format=json 2>/dev/null) || die "cargo build $* -p $PKG failed ($label)"
  printf '%s' "$json" | report_features "$label" || rc=1
  bin=$(printf '%s' "$json" | bin_from_json | head -1)
  [ -n "$bin" ] || { echo "FAIL ($label): no bin artifact in cargo output"; return 1; }
  byte_legs "$label" "$bin" || rc=1
  [ "$rc" -eq 0 ] && echo "ok ($label): graph, feature set, marker and reference legs clean"
  return "$rc"
}

BAZEL=${DC_BAZEL:-scripts/bazel.sh}
BAZEL_TARGET=//crates/cli:disk-cleanup

# Mirror of leg_profile for the Bazel side: --config=dist is opt plus thin LTO, as [profile.dist] is release plus thin.
leg_bazel_profile() {
  grep -Eq '^build:dist +--compilation_mode=opt *$' "${1:-.bazelrc}" &&
    grep -Eq '^build:dist +--@rules_rust//rust/settings:lto=thin *$' "${1:-.bazelrc}"
}

# stdin: `bazel aquery 'mnemonic("Rustc", deps(...))' --output=jsonproto`. Same exit codes as leg_features: the core
# crate's compile action must be present (a graph that never compiles core proves nothing) and none may be given
# the feature (rules_rust passes `--cfg` and `feature="test-support"` as separate arguments).
leg_bazel_features() {
  local out
  out=$(jq -r --arg c "--crate-name=$CORE_TARGET" '[.actions[]? | select(.mnemonic=="Rustc")] as $a
        | if ($a | map(select(.arguments | index($c))) | length) == 0 then "none"
          elif any($a[]; .arguments | index("feature=\"test-support\"")) then "taint" else "clean" end') || return 2
  case "$out" in clean) return 0 ;; taint) return 1 ;; *) return 2 ;; esac
}

bazel_dist_binary() {  # builds the dist binary, prints its absolute path
  local rel root
  "$BAZEL" build --config=dist "$@" "$BAZEL_TARGET" >&2 || return 1
  rel=$("$BAZEL" cquery --config=dist "$@" --output=files "$BAZEL_TARGET" 2>/dev/null | head -1) || return 1
  root=$("$BAZEL" info --config=dist "$@" execution_root 2>/dev/null) || return 1
  [ -n "$rel" ] && [ -f "$root/$rel" ] && printf '%s\n' "$root/$rel"
}

bazel_check() {  # real Bazel leg: aquery graph leg, then the byte legs on the dist binary
  local rc=0 trc=0 bin
  leg_bazel_profile .bazelrc || { echo 'FAIL bazel profile leg: .bazelrc must define build:dist as --compilation_mode=opt plus lto=thin'; rc=1; }
  "$BAZEL" aquery --config=dist "mnemonic(\"Rustc\", deps($BAZEL_TARGET))" --output=jsonproto 2>/dev/null | leg_bazel_features || trc=$?
  case $trc in 0) ;; 1) echo "FAIL bazel feature leg: a Rustc action in deps($BAZEL_TARGET) has feature test-support"; rc=1 ;;
    *) echo "FAIL bazel feature leg: no Rustc action for $CORE_TARGET in the aquery output, or aquery failed"; rc=1 ;; esac
  bin=$(bazel_dist_binary) || { echo "FAIL (bazel): could not build $BAZEL_TARGET with --config=dist"; return 1; }
  byte_legs bazel "$bin" || rc=1
  [ "$rc" -eq 0 ] && echo "ok (bazel dist): profile, feature set, marker and reference legs clean ($bin)"
  return "$rc"
}

bazel_selftest() {
  need jq; local rc=0 r bin art='{"mnemonic":"Rustc","arguments":["--crate-name='"$CORE_TARGET"'"'
  r=0; printf '{"actions":[%s,"--cfg","feature=\\"test-support\\""]}]}\n' "$art" | leg_bazel_features || r=$?; [ "$r" -eq 1 ] && echo "ok: bazel feature leg fails (1) on a test-support Rustc action" || { echo "SELFTEST BAD: bazel feature leg missed (rc $r)"; rc=1; }
  r=0; printf '{"actions":[%s,"--cfg","feature=\\"default\\""]}]}\n' "$art" | leg_bazel_features || r=$?; [ "$r" -eq 0 ] && echo "ok: bazel feature leg passes on a clean action" || { echo "SELFTEST BAD: bazel feature leg false positive (rc $r)"; rc=1; }
  r=0; printf '{"actions":[{"mnemonic":"Rustc","arguments":["--crate-name=other"]}]}\n' | leg_bazel_features || r=$?; [ "$r" -eq 2 ] && echo "ok: bazel feature leg fails (2) when no core action is present" || { echo "SELFTEST BAD: missing core action passed (rc $r)"; rc=1; }
  r=0; printf '{}\n' | leg_bazel_features || r=$?; [ "$r" -eq 2 ] && echo "ok: bazel feature leg fails (2) on empty aquery output" || { echo "SELFTEST BAD: empty aquery passed (rc $r)"; rc=1; }
  r=0; printf 'not json\n' | leg_bazel_features 2>/dev/null || r=$?; [ "$r" -eq 2 ] && echo "ok: bazel feature leg fails (2) on garbage" || { echo "SELFTEST BAD: garbage passed (rc $r)"; rc=1; }
  TAINT_DIR=$(mktemp -d); local t="$TAINT_DIR"
  printf 'build:dist --compilation_mode=opt\nbuild:dist --@rules_rust//rust/settings:lto=thin\n' > "$t/rc"; leg_bazel_profile "$t/rc" && echo "ok: dist config with opt and thin LTO accepted" || { echo "SELFTEST BAD: dist config rejected"; rc=1; }
  printf 'build:dist --compilation_mode=opt\n' > "$t/rc"; leg_bazel_profile "$t/rc" && { echo "SELFTEST BAD: dist config without thin LTO accepted"; rc=1; } || echo "ok: dist config without thin LTO rejected"
  printf 'build:other --compilation_mode=opt\nbuild:other --@rules_rust//rust/settings:lto=thin\n' > "$t/rc"; leg_bazel_profile "$t/rc" && { echo "SELFTEST BAD: settings of another config accepted"; rc=1; } || echo "ok: settings of another config do not satisfy build:dist"
  # end to end: a deliberately tainted dist build (the //build:test_support flag) must fail every byte leg and the aquery leg
  r=0; "$BAZEL" aquery --config=dist --//build:test_support=true "mnemonic(\"Rustc\", deps($BAZEL_TARGET))" --output=jsonproto 2>/dev/null | leg_bazel_features || r=$?
  [ "$r" -eq 1 ] && echo "ok: tainted bazel build, aquery feature leg fails" || { echo "SELFTEST BAD: tainted bazel build, aquery feature leg passed (rc $r)"; rc=1; }
  if bin=$(bazel_dist_binary --//build:test_support=true); then
    scanner_sees_marker "$bin" && echo "ok: tainted bazel build, marker found (scanner is not blind)" || { echo "SELFTEST BAD: marker absent from a tainted bazel binary"; rc=1; }
    leg_marker "$bin" && { echo "SELFTEST BAD: tainted bazel build, marker leg passed"; rc=1; } || echo "ok: tainted bazel build, marker leg fails"
    scanner_sees_panic_marker "$bin" && echo "ok: tainted bazel build, $PANIC_MARKER and $DELAY_MARKER found (scanner is not blind)" || { echo "SELFTEST BAD: a test-support switch absent from a tainted bazel binary"; rc=1; }
    if gbin=$(bazel_dist_binary --//build:test_support=true --config=gate-d); then
      scanner_sees_delete_marker "$gbin" && echo "ok: tainted gate-d bazel build, $DELETE_MARKER found (scanner is not blind)" || { echo "SELFTEST BAD: $DELETE_MARKER absent from a tainted gate-d bazel binary"; rc=1; }
      leg_panic_marker "$gbin" && { echo "SELFTEST BAD: tainted gate-d bazel build, marker leg passed"; rc=1; } || echo "ok: tainted gate-d bazel build, marker leg fails"
    else echo "SELFTEST BAD: tainted gate-d bazel build failed to build"; rc=1; fi
    leg_panic_marker "$bin" && { echo "SELFTEST BAD: tainted bazel build, panic marker leg passed"; rc=1; } || echo "ok: tainted bazel build, panic marker leg fails"
  else echo "SELFTEST BAD: tainted bazel build failed to build"; rc=1; fi
  return "$rc"
}

cleanup_selftest() { [ -n "$TAINT_DIR" ] && rm -rf "$TAINT_DIR"; return 0; }

selftest() {
  need jq; need cargo
  TAINT_DIR=$(mktemp -d); trap cleanup_selftest EXIT INT TERM
  local t="$TAINT_DIR" rc=0
  # 1 each leg fails alone on crafted input, and passes on clean input
  printf 'disk-cleanup-core v0.1.0 (/w/crates/core)\n└── disk-cleanup-core feature "test-support" (command-line)\n' | leg_graph && { echo "SELFTEST BAD: graph leg missed"; rc=1; } || echo "ok: graph leg fails on a test-support edge"
  printf 'disk-cleanup-core v0.1.0 (/w/crates/core)\n└── disk-cleanup-core feature "default"\n' | leg_graph && echo "ok: graph leg passes on a clean tree" || { echo "SELFTEST BAD: graph leg false positive"; rc=1; }
  local r=0; printf 'nothing here\n' | leg_graph || r=$?; [ "$r" -eq 2 ] && echo "ok: graph leg fails (2) on a tree with no core node" || { echo "SELFTEST BAD: graph leg passed an empty tree"; rc=1; }
  local art='"reason":"compiler-artifact","target":{"name":"disk_cleanup_core","kind":["lib"]}'
  r=0; printf '{%s,"package_id":"path+file:///w/crates/core#disk-cleanup-core@0.1.0","features":["test-support"]}\n' "$art" | leg_features || r=$?; [ "$r" -eq 1 ] && echo "ok: feature leg fails (1) on a test-support artifact" || { echo "SELFTEST BAD: feature leg missed (rc $r)"; rc=1; }
  r=0; printf '{%s,"package_id":"path+file:///w/crates/core#disk-cleanup-core@0.1.0","features":[]}\n' "$art" | leg_features || r=$?; [ "$r" -eq 0 ] && echo "ok: feature leg passes on a clean artifact" || { echo "SELFTEST BAD: feature leg false positive (rc $r)"; rc=1; }
  r=0; printf '{%s,"package_id":"path+file:///w/disk-cleanup-core#0.1.0","features":["test-support"]}\n' "$art" | leg_features || r=$?; [ "$r" -eq 1 ] && echo "ok: feature leg fails on a package_id without name@version (directory named like the package)" || { echo "SELFTEST BAD: nameless package_id evades the feature leg (rc $r)"; rc=1; }
  r=0; printf '' | leg_features || r=$?; [ "$r" -eq 2 ] && echo "ok: feature leg fails (2) on empty input" || { echo "SELFTEST BAD: empty input passed (rc $r)"; rc=1; }
  r=0; printf 'not json\n' | leg_features 2>/dev/null || r=$?; [ "$r" -eq 2 ] && echo "ok: feature leg fails (2) on garbage input" || { echo "SELFTEST BAD: garbage input passed (rc $r)"; rc=1; }
  r=0; printf '{"reason":"compiler-artifact","target":{"name":"other","kind":["lib"]},"features":[]}\n' | leg_features || r=$?; [ "$r" -eq 2 ] && echo "ok: feature leg fails (2) when no core artifact is present" || { echo "SELFTEST BAD: missing core artifact passed (rc $r)"; rc=1; }
  printf 'ELF\0pad%spad' "$MARKER" > "$t/tainted"; printf 'ELF\0pad%spad' 'dc-flavor=release' > "$t/clean"; printf 'ELF\0pad' > "$t/blind"
  printf 'ELF\0pad%spad%spad' 'dc-flavor=release' "$PANIC_MARKER" > "$t/panicky"; printf 'ELF\0pad%spad%spad' 'dc-flavor=release' "$DELAY_MARKER" > "$t/slowpoke"; printf 'ELF\0pad%spad%spad' 'dc-flavor=release' "$DELETE_MARKER" > "$t/slowdel"
  leg_panic_marker "$t/slowdel" && { echo "SELFTEST BAD: slow-delete marker leg missed"; rc=1; } || echo "ok: marker leg fails on a binary holding $DELETE_MARKER"
  leg_panic_marker "$t/slowpoke" && { echo "SELFTEST BAD: slow-planner marker leg missed"; rc=1; } || echo "ok: marker leg fails on a binary holding $DELAY_MARKER"
  leg_panic_marker "$t/panicky" && { echo "SELFTEST BAD: panic marker leg missed"; rc=1; } || echo "ok: panic marker leg fails on a binary holding $PANIC_MARKER"
  leg_panic_marker "$t/clean" && echo "ok: panic marker leg passes on clean bytes" || { echo "SELFTEST BAD: panic marker leg false positive"; rc=1; }
  leg_marker "$t/tainted" && { echo "SELFTEST BAD: marker leg missed"; rc=1; } || echo "ok: marker leg fails on tainted bytes"
  leg_marker "$t/clean" && echo "ok: marker leg passes on clean bytes" || { echo "SELFTEST BAD: marker leg false positive"; rc=1; }
  leg_reference "$t/clean" && echo "ok: reference leg passes when a flavor string is present" || { echo "SELFTEST BAD: reference leg false positive"; rc=1; }
  leg_reference "$t/blind" && { echo "SELFTEST BAD: reference leg passed a binary that never reaches build_flavor()"; rc=1; } || echo "ok: reference leg fails on a binary without the flavor string"
  # 2 end to end in a separate target dir, so a tainted binary can never land in target/release
  local json bin tree
  tree=$(CARGO_TARGET_DIR="$t/target" cargo tree -p "$PKG" -e normal,build,features -i "$CORE" --target all --features "$CORE/test-support")
  printf '%s' "$tree" | leg_graph && { echo "SELFTEST BAD: forced build, graph leg passed"; rc=1; } || echo "ok: forced build, graph leg fails"
  json=$(CARGO_TARGET_DIR="$t/target" cargo build --release -p "$PKG" --features "$CORE/test-support" --message-format=json 2>/dev/null)
  [ "${DC_SELFTEST_FAULT:-}" = after-forced-build ] && { echo "selftest: injected fault after the forced build"; false; }
  printf '%s' "$json" | leg_features && { echo "SELFTEST BAD: forced build, feature leg passed"; rc=1; } || echo "ok: forced build, feature leg fails"
  bin=$(printf '%s' "$json" | bin_from_json | head -1)
  scanner_sees_marker "$bin" && echo "ok: forced build, marker found in $bin (scanner is not blind)" || { echo "SELFTEST BAD: marker absent from a test-support binary: scanner blind"; rc=1; }
  leg_marker "$bin" && { echo "SELFTEST BAD: forced build, marker leg passed"; rc=1; } || echo "ok: forced build, marker leg fails"
  scanner_sees_panic_marker "$bin" && echo "ok: forced build, $PANIC_MARKER and $DELAY_MARKER found (scanner is not blind)" || { echo "SELFTEST BAD: a test-support switch absent from a test-support binary: scanner blind"; rc=1; }
  leg_panic_marker "$bin" && { echo "SELFTEST BAD: forced build, panic marker leg passed"; rc=1; } || echo "ok: forced build, panic marker leg fails"
  json=$(CARGO_TARGET_DIR="$t/target" cargo build --release -p "$PKG" --features "$CORE/test-support,$PKG/gate-d" --message-format=json 2>/dev/null)
  bin=$(printf '%s' "$json" | bin_from_json | head -1)
  scanner_sees_delete_marker "$bin" && echo "ok: forced gate-d build, $DELETE_MARKER found (scanner is not blind)" || { echo "SELFTEST BAD: $DELETE_MARKER absent from a forced gate-d test-support binary: scanner blind"; rc=1; }
  leg_panic_marker "$bin" && { echo "SELFTEST BAD: forced gate-d build, marker leg passed"; rc=1; } || echo "ok: forced gate-d build, marker leg fails"
  # 3 manifest pins
  printf '[workspace]\nresolver = "1"\n' > "$t/Cargo.toml"
  leg_resolver "$t/Cargo.toml" && { echo "SELFTEST BAD: resolver 1 accepted"; rc=1; } || echo "ok: resolver 1 rejected"
  leg_profile "$t/Cargo.toml" && { echo "SELFTEST BAD: manifest without [profile.dist] accepted"; rc=1; } || echo "ok: manifest without [profile.dist] rejected"
  printf '[profile.dist]\ninherits = "release"\nlto = "thin"\n' > "$t/Cargo.toml"
  leg_profile "$t/Cargo.toml" && echo "ok: [profile.dist] inheriting release accepted" || { echo "SELFTEST BAD: [profile.dist] rejected"; rc=1; }
  printf '[profile.dist]\nlto = "thin"\n\n[profile.other]\ninherits = "release"\n' > "$t/Cargo.toml"
  leg_profile "$t/Cargo.toml" && { echo "SELFTEST BAD: inherits in another table accepted"; rc=1; } || echo "ok: inherits = \"release\" in another table does not satisfy [profile.dist]"
  printf '[profile.dist]\ninherits = "dev"\n\n[profile.other]\ninherits = "release"\n' > "$t/Cargo.toml"
  leg_profile "$t/Cargo.toml" && { echo "SELFTEST BAD: [profile.dist] inheriting dev accepted"; rc=1; } || echo "ok: [profile.dist] inheriting something other than release is rejected"
  printf '[profile.other]\ninherits = "release"\n\n[profile.dist]  # cargo-dist\nlto = "thin"\ninherits = "release"  # keep\n' > "$t/Cargo.toml"
  leg_profile "$t/Cargo.toml" && echo "ok: inherits after other keys and trailing comments in [profile.dist] accepted" || { echo "SELFTEST BAD: valid [profile.dist] with comments rejected"; rc=1; }
  return "$rc"
}

trap cleanup_selftest EXIT INT TERM
case "${1:-}" in
  --selftest) selftest ;;
  --bazel-selftest) bazel_selftest ;;
  --artifact) need grep; [ -f "${2:?path}" ] || die "$2 is not a file"; byte_legs artifact "$2" && echo "ok: no marker in $2 and main reaches build_flavor()" || exit 1 ;;
  "") need jq; legs=${DC_RELEASE_FEATURES_LEGS:-both}; rc=0
      case $legs in cargo | bazel | both) ;; *) die "DC_RELEASE_FEATURES_LEGS must be cargo, bazel or both" ;; esac
      if [ "$legs" != bazel ]; then
        need cargo
        leg_resolver Cargo.toml || die 'workspace Cargo.toml must pin resolver = "2"'
        leg_profile Cargo.toml || die 'workspace Cargo.toml must define [profile.dist] with inherits = "release" (Task 0.1.1b)'
        build_and_check --release --release || rc=1; build_and_check "--profile dist" --profile dist || rc=1
      fi
      if [ "$legs" != cargo ]; then
        [ -x "$BAZEL" ] || command -v "$BAZEL" >/dev/null 2>&1 || die "$BAZEL not found (the Bazel leg cannot run, so it does not pass; DC_RELEASE_FEATURES_LEGS=cargo skips it)"
        bazel_check || rc=1
      fi
      exit "$rc" ;;
  *) echo "usage: $0 [--artifact BIN | --selftest | --bazel-selftest]" >&2; exit 64 ;;
esac

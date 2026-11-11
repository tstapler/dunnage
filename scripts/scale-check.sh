#!/usr/bin/env bash
# scale-check.sh [--smoke] [--bin PATH] | --selftest
# Story 1.3.3 (validation T-320): builds a tree of 1,000,000 inodes with 5,000 build dirs under a DC_TEST_ROOT
# (a mktemp directory on tmpfs when /dev/shm exists, so the owner's disk is not filled with inodes), runs the dry
# run (HOME = the tree) and `disk-cleanup scan` over it under /usr/bin/time -v, and requires exit 0 and a peak
# resident set under 512 MiB for each. Local and nightly; --smoke builds 20,000 inodes with 100 build dirs.
# Without /usr/bin/time it falls back to python3 resource.getrusage and prints the same line.
# Overrides: DC_SCALE_INODES, DC_SCALE_BUILD_DIRS, DC_SCALE_RSS_MIB, DC_SCALE_PARENT, DC_BENCH_BIN.
set -u -o pipefail

INODES=${DC_SCALE_INODES:-1000000}
BUILD_DIRS=${DC_SCALE_BUILD_DIRS:-5000}
RSS_MIB=${DC_SCALE_RSS_MIB:-512}
SENTINEL=.dc-test-root

say() { printf '%s\n' "$*"; }
fail() { say "FAIL: $*"; exit 1; }

# generate <root>: <root>/home/code/p<i>/{package.json, node_modules/<pkg>/<9 files>, loose files}, K inodes each.
generate() {
  local root=$1 per pkgs loose
  [ -e "$root/$SENTINEL" ] || fail "$root has no $SENTINEL; refusing to build a tree there"
  per=$(( INODES / BUILD_DIRS ))
  [ "$per" -ge 13 ] || fail "need at least 13 inodes per build dir (have $per)"
  pkgs=$(( (per - 3) / 10 )); loose=$(( per - 3 - pkgs * 10 ))
  mkdir -p "$root/home/code" "$root/xdg/state" "$root/xdg/cache" "$root/xdg/config"
  # directories first, then files, both through xargs so a million entries take seconds
  awk -v b="$BUILD_DIRS" -v p="$pkgs" -v r="$root/home/code" 'BEGIN {
    for (i = 0; i < b; i++) for (j = 0; j < p; j++) print r "/p" i "/node_modules/k" j }' | xargs -P8 -n 2000 mkdir -p
  awk -v b="$BUILD_DIRS" -v p="$pkgs" -v l="$loose" -v r="$root/home/code" 'BEGIN {
    for (i = 0; i < b; i++) {
      print r "/p" i "/package.json"
      for (k = 0; k < l; k++) print r "/p" i "/src" k ".js"
      for (j = 0; j < p; j++) for (f = 0; f < 9; f++) print r "/p" i "/node_modules/k" j "/f" f ".js" } }' | xargs -P8 -n 2000 touch
  sleep "${DC_SCALE_RACY_SLEEP:-3}"
}

count_inodes() { find "$1" -mindepth 1 | wc -l; }

# timed <out-file> <cmd...>: runs the command, writes "Maximum resident set size (kbytes): N" and the exit status.
timed() {
  local out=$1; shift
  if [ -x /usr/bin/time ]; then
    /usr/bin/time -v -o "$out" "$@" > /dev/null 2>&1; local rc=$?
  else
    python3 - "$out" "$@" <<'PY'
import resource, subprocess, sys
out, cmd = sys.argv[1], sys.argv[2:]
rc = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode
kb = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
open(out, "w").write(f"\tMaximum resident set size (kbytes): {kb}\n\tExit status: {rc}\n")
sys.exit(rc)
PY
    local rc=$?
  fi
  return $rc
}

peak_mib() { awk -F': ' '/Maximum resident set size/ {printf "%d", ($2 + 1023) / 1024}' "$1"; }

# check_run <label> <time-file> <rc>
check_run() {
  local label=$1 file=$2 rc=$3 mib
  mib=$(peak_mib "$file")
  say "$label: exit $rc, peak RSS ${mib:-?} MiB (limit $RSS_MIB MiB)"
  [ "$rc" = 0 ] || fail "$label exited $rc"
  [ -n "$mib" ] || fail "$label: no peak RSS in $file"
  [ "$mib" -lt "$RSS_MIB" ] || fail "$label: peak RSS $mib MiB is not under $RSS_MIB MiB"
}

run() {
  local bin=$1 parent root rc n t0
  [ -x "$bin" ] || { say "binary not found: $bin (cargo build --release)"; exit 64; }
  parent=${DC_SCALE_PARENT:-$([ -d /dev/shm ] && echo /dev/shm || echo "${TMPDIR:-/tmp}")}
  root=$(mktemp -d "$parent/dc-scale.XXXXXX") || fail "cannot create a directory under $parent"
  : > "$root/$SENTINEL"
  # shellcheck disable=SC2064
  trap "case '$root' in '$parent'/dc-scale.*) [ -e '$root/$SENTINEL' ] && rm -rf -- '$root';; esac" EXIT
  t0=$(date +%s); generate "$root"
  n=$(count_inodes "$root")
  say "tree: $n inodes ($BUILD_DIRS build dirs) under $root, built in $(( $(date +%s) - t0 )) s"
  [ "$n" -ge "$INODES" ] || fail "built $n inodes, wanted $INODES"
  local -a env_run=(env -i PATH=/usr/bin:/bin HOME="$root/home" XDG_STATE_HOME="$root/xdg/state" XDG_CACHE_HOME="$root/xdg/cache" XDG_CONFIG_HOME="$root/xdg/config" DC_TEST_ROOT="$root" LANG=C)
  timed "$root/time-report" "${env_run[@]}" "$bin" --json --no-progress; rc=$?
  check_run "dry run (plan mode, HOME = the tree)" "$root/time-report" $rc
  timed "$root/time-scan" "${env_run[@]}" "$bin" scan --backend walker --cache-file "$root/xdg/cache/scan.cache" --json "$root/home/code"; rc=$?
  check_run "scan, cold walker cache" "$root/time-scan" $rc
  timed "$root/time-scan2" "${env_run[@]}" "$bin" scan --backend walker --cache-file "$root/xdg/cache/scan.cache" --json "$root/home/code"; rc=$?
  check_run "scan, warm walker cache" "$root/time-scan2" $rc
  say "PASS"
}

selftest() {
  local self bad=0 n=0 out rc d
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "${d:-}"' EXIT
  expect() { local want=$1 name=$2 pat=$3; shift 3; out=$("$@" 2>&1); rc=$?
    if [ "$rc" = "$want" ] && echo "$out" | grep -q -- "$pat"; then n=$((n+1)); echo "ok: $name"; else echo "SELFTEST BAD: $name (rc=$rc, want $want, '$pat'): $out"; bad=$((bad+1)); fi; }
  printf '#!/bin/sh\necho "{}"\n' > "$d/small"
  printf '#!/bin/sh\npython3 -c "x = b\\"x\\" * (60 * 1024 * 1024); print(len(x))" > /dev/null\n' > "$d/big"
  printf '#!/bin/sh\nexit 3\n' > "$d/dies"
  chmod +x "$d/small" "$d/big" "$d/dies"
  export DC_SCALE_INODES=2600 DC_SCALE_BUILD_DIRS=20 DC_SCALE_RACY_SLEEP=0 DC_SCALE_PARENT="$d"
  expect 0 "a small binary passes and the tree holds the requested inodes" "PASS" "$self" --bin "$d/small"
  expect 0 "the tree is reported with its inode count" "tree: 2[0-9][0-9][0-9] inodes (20 build dirs)" "$self" --bin "$d/small"
  expect 1 "a binary over the RSS limit fails" "peak RSS .* is not under 40 MiB" env DC_SCALE_RSS_MIB=40 "$self" --bin "$d/big"
  expect 1 "a binary that exits non-zero fails" "exited 3" "$self" --bin "$d/dies"
  expect 1 "too few inodes per build dir is refused" "at least 13 inodes" env DC_SCALE_INODES=100 "$self" --bin "$d/small"
  expect 64 "a missing binary is a usage error" "binary not found" "$self" --bin "$d/none"
  expect 1 "a root without the sentinel is refused" "refusing to build" "$self" --generate-only "$d"
  if ls "$d"/dc-scale.* >/dev/null 2>&1; then echo "SELFTEST BAD: a run left its tree behind"; bad=$((bad+1)); else n=$((n+1)); echo "ok: every run removed its own tree"; fi
  [ "$bad" = 0 ] || { echo "scale-check selftest: $bad failed"; exit 1; }
  echo "scale-check selftest: $n ok"
}

bin=${DC_BENCH_BIN:-${CARGO_TARGET_DIR:-target}/release/disk-cleanup}
while [ $# -gt 0 ]; do
  case $1 in
    --selftest) selftest; exit $? ;;
    --generate-only) shift; generate "${1:-}"; exit 0 ;;
    --smoke) INODES=${DC_SCALE_INODES:-20000}; BUILD_DIRS=${DC_SCALE_BUILD_DIRS:-100} ;;
    --bin) shift; bin=${1:-}; [ -n "$bin" ] || { echo "usage: $0 [--smoke] [--bin PATH] | --selftest"; exit 64; } ;;
    *) echo "usage: $0 [--smoke] [--bin PATH] | --selftest"; exit 64 ;;
  esac
  shift
done
if [ "${DC_BENCH_BAZEL:-0}" = 1 ] && [ -z "${DC_BENCH_BIN:-}" ] && [ "$bin" = "${CARGO_TARGET_DIR:-target}/release/disk-cleanup" ]; then
  bin=$(cd "$(dirname "$0")/.." && scripts/gate-binary.sh none) || fail "bazel build failed"   # set by scripts/gate.sh
fi
[ -n "${DC_BENCH_BIN:-}" ] || [ "$bin" != "${CARGO_TARGET_DIR:-target}/release/disk-cleanup" ] || [ "${DC_BENCH_BAZEL:-0}" = 1 ] || cargo build --release --locked -q || fail "cargo build --release failed"
run "$bin"

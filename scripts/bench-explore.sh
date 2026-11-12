#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2016
# bench-explore.sh [--bin PATH] [--smoke] | --selftest
# Story 6.5.6 (Task 6.5.6b, Gate F): "the explorer stays responsive on the largest tree". Runs the real binary on
# a pty (crates/cli/tests/explore_bench.rs) over a generated tree of 676,296 directories (the shape measured in
# SA s3): cold (no cache), against a synthetic stale cache of the same directories, and warm. Asserts
#   first frame within 500 ms of spawn (CI bound 1,500 ms: DC_BENCH_CI=1 or CI set),
#   spawn to the fully painted cached tree within 3 s (synthetic cache and warm cache),
#   the longest key-to-frame wait while scanning under 100 ms,
#   at most 4 draws per second while scanning with no key,
#   ExploreTree::heap_bytes() per directory at most 160 fresh and 200 while revalidating (core/tests/explore_perf.rs),
# and reports peak RSS (VmHWM, not asserted) and the cold scan time. The numbers go to research/spikes.md
# (DC_SPIKES_FILE, default ~/dotfiles/project_plans/rust-disk-cleaner/research/spikes.md when that file exists).
#   --smoke     2,000 directories, no time assertion: the harness works and prints every number
#   --selftest  the assertions over canned numbers (one pass, one failure per bound)
#   DC_BENCH_OWNER_MACHINE=1  adds a cold and a warm run over the real $HOME (read-only) next to the generated tree
# Overrides: DC_BENCH_BIN / --bin (default: cargo build --release), DC_BENCH_MAX_LOAD (default nproc/2: above it
# the run is INCONCLUSIVE, exit 77, and says so), DC_BENCH_EXPLORE_DIRS, DC_BENCH_LOADAVG_FILE, DC_BENCH_*_MS bounds.
# Exit 0 pass, 1 fail, 77 INCONCLUSIVE (load), 64 usage.
set -u -o pipefail

LOADAVG_FILE=${DC_BENCH_LOADAVG_FILE:-/proc/loadavg}
MAX_LOAD=${DC_BENCH_MAX_LOAD:-$(( $(nproc) / 2 ))}
if [ -n "${DC_BENCH_CI:-${CI:-}}" ]; then FIRST_FRAME_MS=${DC_BENCH_FIRST_FRAME_MS:-1500}; else FIRST_FRAME_MS=${DC_BENCH_FIRST_FRAME_MS:-500}; fi
PAINT_MS=${DC_BENCH_PAINT_MS:-3000}
FRAME_MS=${DC_BENCH_FRAME_MS:-100}
DRAWS_PER_S=${DC_BENCH_DRAWS_PER_S:-4}
HEAP_FRESH=${DC_BENCH_HEAP_FRESH:-160}
HEAP_PAINTED=${DC_BENCH_HEAP_PAINTED:-200}
SPIKES=${DC_SPIKES_FILE:-$HOME/dotfiles/project_plans/rust-disk-cleaner/research/spikes.md}

say() { printf '%s\n' "$*"; }

# stdin: BENCH name=value lines. Prints one FAIL line per violated bound; exit 1 if any, 2 if a number is missing.
check_numbers() {
  awk -v ff="$FIRST_FRAME_MS" -v pt="$PAINT_MS" -v fm="$FRAME_MS" -v dr="$DRAWS_PER_S" -v hf="$HEAP_FRESH" -v hp="$HEAP_PAINTED" '
    /^BENCH / { sub(/^BENCH /, ""); n = index($0, "="); v[substr($0, 1, n - 1)] = substr($0, n + 1) }
    function need(k) { if (!(k in v)) { printf "MISSING: %s\n", k; miss = 1; return 0 } return 1 }
    function atmost(k, bound, what) { if (need(k) && v[k] + 0 > bound) { printf "FAIL: %s = %s, %s\n", k, v[k], what; bad = 1 } }
    END {
      atmost("cold_first_frame_ms", ff, "bound " ff " ms (first frame after spawn)")
      atmost("stale_first_frame_ms", ff, "bound " ff " ms (first frame after spawn)")
      atmost("warm_first_frame_ms", ff, "bound " ff " ms (first frame after spawn)")
      atmost("stale_painted_ms", pt, "bound " pt " ms (spawn to the painted cached tree)")
      atmost("warm_painted_ms", pt, "bound " pt " ms (spawn to the painted cached tree)")
      atmost("stale_key_to_frame_max_ms", fm, "bound " fm " ms (key to next frame while scanning)")
      atmost("cold_key_to_frame_max_ms", fm, "bound " fm " ms (key to next frame while scanning)")
      atmost("cold_draws_per_s", dr, "bound " dr " draws per second with no key")
      atmost("warm_draws_per_s", dr, "bound " dr " draws per second with no key")
      atmost("heap_bytes_per_dir_fresh", hf, "bound " hf " bytes per directory")
      atmost("heap_bytes_per_dir_painted", hp, "bound " hp " bytes per directory while revalidating")
      need("cold_peak_rss_mib"); need("cold_scan_done_s"); need("tree_dirs")
      exit (miss ? 2 : (bad ? 1 : 0))
    }'
}

selftest() {
  local bad=0 rc good
  good='BENCH tree_dirs=676296
BENCH cold_first_frame_ms=300
BENCH stale_first_frame_ms=310
BENCH warm_first_frame_ms=290
BENCH stale_painted_ms=2500
BENCH warm_painted_ms=2400
BENCH stale_key_to_frame_max_ms=40
BENCH cold_key_to_frame_max_ms=60
BENCH cold_draws_per_s=3.90
BENCH warm_draws_per_s=2.10
BENCH heap_bytes_per_dir_fresh=150.0
BENCH heap_bytes_per_dir_painted=190.0
BENCH cold_peak_rss_mib=900
BENCH cold_scan_done_s=21.5'
  expect() { # name want-rc input
    local out; out=$(printf '%s\n' "$3" | check_numbers); rc=$?
    if [ "$rc" = "$2" ]; then say "ok: $1"; else say "SELFTEST BAD: $1 (rc $rc, want $2): $out"; bad=1; fi
  }
  expect "numbers inside every bound pass" 0 "$good"
  expect "a late first frame fails" 1 "${good/cold_first_frame_ms=300/cold_first_frame_ms=501}"
  expect "a slow painted tree fails" 1 "${good/warm_painted_ms=2400/warm_painted_ms=3001}"
  expect "a 101 ms key wait fails (bound 100)" 1 "${good/cold_key_to_frame_max_ms=60/cold_key_to_frame_max_ms=101}"
  expect "more than 4 draws per second fails" 1 "${good/cold_draws_per_s=3.90/cold_draws_per_s=4.5}"
  expect "161 bytes per directory fresh fails" 1 "${good/heap_bytes_per_dir_fresh=150.0/heap_bytes_per_dir_fresh=161.0}"
  expect "a missing number is not a pass" 2 "$(printf '%s\n' "$good" | grep -v warm_painted_ms)"
  expect "empty input is not a pass" 2 ""
  local old=$LOADAVG_FILE oldmax=$MAX_LOAD t; t=$(mktemp); echo "40.00 20.00 10.00 1/100 1" > "$t"
  MAX_LOAD=8   # the guard under test, whatever DC_BENCH_MAX_LOAD the caller (the gate) exported
  LOADAVG_FILE=$t; load_ok && { say "SELFTEST BAD: a load of 40 passed"; bad=1; } || say "ok: a load above the limit is INCONCLUSIVE"
  echo "0.50 0.50 0.50 1/100 1" > "$t"; load_ok && say "ok: a quiet machine passes" || { say "SELFTEST BAD: a load of 0.5 failed"; bad=1; }
  LOADAVG_FILE=$old; MAX_LOAD=$oldmax; rm -f "$t"
  [ "$bad" = 0 ] && { say "selftest: all ok"; return 0; } || return 1
}

load_ok() { local l; l=$(cut -d' ' -f1 "$LOADAVG_FILE"); awk -v l="$l" -v m="$MAX_LOAD" 'BEGIN { exit !(l + 0 <= m + 0) }'; }

write_spikes() { # $1 = results file, $2 = load line, $3 = binary label
  local sha when body
  sha=$(git rev-parse --short HEAD 2>/dev/null || echo unknown); when=$(date +%F)
  body=$(printf '\n## Story 6.5.6: Gate F explorer bench (%s, DC commit %s)\n\n' "$when" "$sha"
    printf 'Run by `scripts/bench-explore.sh` (crates/cli/tests/explore_bench.rs) on %s, binary: %s.\n' "$(uname -srm)" "$3"
    printf 'Load average at start and end: %s. `DC_BENCH_MAX_LOAD=%s`. The tree is real directories on %s; each number below is one run.\n\n' "$2" "$MAX_LOAD" "$(df -T "${TMPDIR:-/tmp}" | awk 'NR==2 {print $2}')"
    printf '| number | value |\n|---|---|\n'
    sed -e 's/^BENCH //' -e 's/^\([^=]*\)=\(.*\)$/| \1 | \2 |/' "$1"
    printf '\n')
  if [ -f "$SPIKES" ]; then printf '%s\n' "$body" >> "$SPIKES"; say "numbers appended to $SPIKES"; else say "no $SPIKES; numbers not written"; fi
}

run() {
  local bin=${1:-} smoke=${2:-0} res rc=0 load0 load1 logf label
  if [ "$smoke" = 0 ] && ! load_ok; then
    say "INCONCLUSIVE: load average $(cut -d' ' -f1-3 "$LOADAVG_FILE") is above DC_BENCH_MAX_LOAD=$MAX_LOAD (set it higher to run anyway and record the load)"; return 77
  fi
  if [ -z "$bin" ] && [ "$smoke" = 1 ]; then
    bin=''  # the smoke run measures nothing: the harness uses the test build's own binary
  elif [ -z "$bin" ]; then
    cargo build --release -p disk-cleanup >&2 || { say "FAIL: cargo build --release"; return 1; }
    bin=${CARGO_TARGET_DIR:-target}/release/disk-cleanup
  fi
  [ -z "$bin" ] || [ -x "$bin" ] || { say "FAIL: no binary $bin"; return 1; }
  [ -z "$bin" ] || bin=$(readlink -f "$bin")
  res=$(mktemp); logf=$(mktemp)
  load0=$(cut -d' ' -f1-3 "$LOADAVG_FILE")
  if [ "$smoke" = 1 ]; then
    cargo test -p disk-cleanup --test explore_bench -- --nocapture >"$logf" 2>&1 || rc=$?
  else
    DC_BENCH_EXPLORE=1 DC_BENCH_REAL_HOME=${DC_BENCH_OWNER_MACHINE:-0} DC_BENCH_REAL_HOME_PATH=${DC_BENCH_REAL_HOME_PATH:-$HOME} DC_BENCH_BIN=$bin cargo test -p disk-cleanup --test explore_bench -- --nocapture >"$logf" 2>&1 || rc=$?
  fi
  grep '^BENCH ' "$logf" > "$res"; load1=$(cut -d' ' -f1-3 "$LOADAVG_FILE")
  cat "$res"
  [ "$rc" = 0 ] || { say "FAIL: the harness failed (log: $logf)"; tail -20 "$logf"; return 1; }
  if [ "$smoke" = 1 ]; then
    if check_numbers < "$res" | grep -q '^MISSING'; then say "FAIL: smoke: a number is missing"; return 1; fi
    say "ok: smoke (every number printed; no time bound asserted)"; rm -f "$res" "$logf"; return 0
  fi
  check_numbers < "$res"; rc=$?
  label="$bin"
  write_spikes "$res" "$load0 -> $load1" "$label"
  [ "$rc" = 0 ] && say "ok: every bound holds" || say "bench result: rc $rc (see FAIL lines above)"
  rm -f "$res" "$logf"; return "$rc"
}

case "${1:-}" in
  --selftest) selftest ;;
  --smoke) run "${DC_BENCH_BIN:-}" 1 ;;
  --bin) run "${2:?--bin needs a path}" 0 ;;
  "") run "${DC_BENCH_BIN:-}" 0 ;;
  *) say "usage: $0 [--bin PATH | --smoke | --selftest]"; exit 64 ;;
esac

#!/usr/bin/env bash
# bench-dry-run.sh [--bin PATH] | --selftest
# Story 1.3.3 (extended by Story 5.1.1): times the read-only report and asserts the speed protocol of
# requirements.md. 12.7 s is 127 s (the untraced bash baseline) divided by 10.
#   DC_BENCH_OWNER_MACHINE=1  the owner's machine, real HOME: one cold run (reported apart, discarded), 7 warm
#                             runs, the median must be <= 12.7 s, and a --rescan run <= 63.5 s (2x).
#   otherwise                 counters, not seconds: a generated tree of 20,000 directories is scanned twice by
#                             `disk-cleanup scan`; the second run must list nothing and serve every directory
#                             from the cache. No wall-clock assertion.
# Both modes need the report to be one JSON document with the npm and go caches in it: a binary that prints {}
# instantly is not "10x". Exit 0 pass, 1 fail, 77 INCONCLUSIVE (load), 64 usage.
# Overrides for the selftest and for a noisy machine: DC_BENCH_BIN, DC_BENCH_BUDGET_MS, DC_BENCH_RESCAN_BUDGET_MS,
# DC_BENCH_WARM_RUNS, DC_BENCH_MAX_LOAD, DC_BENCH_LOADAVG_FILE, DC_BENCH_DIRS_OUTER, DC_BENCH_DIRS_INNER,
# DC_BENCH_RACY_SLEEP.
set -u -o pipefail

BUDGET_MS=${DC_BENCH_BUDGET_MS:-12700}
RESCAN_BUDGET_MS=${DC_BENCH_RESCAN_BUDGET_MS:-63500}
WARM_RUNS=${DC_BENCH_WARM_RUNS:-7}
FLOOR_BEST_MS=6100
FLOOR_WORST_MS=8100
LOADAVG_FILE=${DC_BENCH_LOADAVG_FILE:-/proc/loadavg}
MAX_LOAD=${DC_BENCH_MAX_LOAD:-$(( $(nproc) / 2 ))}
OUTER=${DC_BENCH_DIRS_OUTER:-100}
INNER=${DC_BENCH_DIRS_INNER:-200}
RACY_SLEEP=${DC_BENCH_RACY_SLEEP:-3}

now_ms() { echo $(( $(date +%s%N) / 1000000 )); }
say() { printf '%s\n' "$*"; }
fail() { say "FAIL: $*"; exit 1; }

# One JSON document, mode plan, exit 0, items present with the npm and go caches among them.
doc_check() {
  local file=$1
  jq -e -s 'length == 1' "$file" >/dev/null 2>&1 || { say "stdout is not exactly one JSON document"; return 1; }
  jq -e '(.items | length) > 0' "$file" >/dev/null || { say "empty document: no item and no skip reason for any kind family"; return 1; }
  jq -e '.mode == "plan" and .exit_code == 0' "$file" >/dev/null || { say "not a plan document with exit_code 0"; return 1; }
  jq -e 'all(.items[]; .family != null and .status != null)' "$file" >/dev/null || { say "an item lacks family or status"; return 1; }
  jq -e 'any(.items[]; .family == "caches" and (.path | endswith("/.npm/_cacache")))' "$file" >/dev/null || { say "no npm cache item"; return 1; }
  jq -e 'any(.items[]; .family == "caches" and (.path | endswith("go-build")))' "$file" >/dev/null || { say "no go cache item"; return 1; }
}

median() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }

load_check() {
  local l1
  read -r l1 _ < "$LOADAVG_FILE" || fail "cannot read $LOADAVG_FILE"
  if awk -v l="$l1" -v m="$MAX_LOAD" 'BEGIN {exit !(l > m)}'; then
    say "INCONCLUSIVE: load (one-minute load average $l1 exceeds $MAX_LOAD; timings on a busy machine prove nothing)"
    exit 77
  fi
}

# Per-phase figures. The plan document of this build carries no timings (Story 5.1.1 adds them), so a miss
# prints what exists: wall time here, the walker's own walk_ms/total_ms from `scan`.
print_phases() {
  local f=$1
  if jq -e '.summary.timings' "$f" >/dev/null 2>&1; then
    say "  discover_ms/plan_ms: $(jq -c '.summary.timings' "$f")"
  else
    say "  discover_ms/plan_ms: not in this build's plan document (see Story 5.1.1)"
  fi
}

owner_mode() {
  local bin=$1 tmp=$2 t0 ms i walls=() med cold rescan_ms walk
  load_check
  say "owner machine: load $(cut -d' ' -f1 "$LOADAVG_FILE"), $(nproc) CPUs, binary $bin"
  # Walker floor: warm `scan` over the default roots (spike 0.2.6 floor F: 6.1 s best, 8.1 s worst).
  "$bin" scan --backend walker --json > "$tmp/scan0.json" 2>/dev/null || true
  "$bin" scan --backend walker --json > "$tmp/scan.json" 2>/dev/null || fail "scan exited non-zero"
  walk=$(jq -r '.walk_ms' "$tmp/scan.json")
  say "walk (warm scan, default roots): walk_ms=$walk total_ms=$(jq -r '.total_ms' "$tmp/scan.json") dirs_listed=$(jq -r '.dirs_listed' "$tmp/scan.json") dirs_cached=$(jq -r '.dirs_cached' "$tmp/scan.json") (floor F ${FLOOR_BEST_MS}-${FLOOR_WORST_MS} ms)"
  if [ "$walk" -gt "$FLOOR_WORST_MS" ]; then say "REGRESSION against the floor: walk ${walk} ms > ${FLOOR_WORST_MS} ms"; fi
  t0=$(now_ms); "$bin" --json --no-progress > "$tmp/cold.json" 2>/dev/null || fail "cold run exited non-zero"
  cold=$(( $(now_ms) - t0 ))
  say "cold run (discarded): ${cold} ms"
  doc_check "$tmp/cold.json" || fail "cold run: document check"
  for i in $(seq 1 "$WARM_RUNS"); do
    t0=$(now_ms); "$bin" --json --no-progress > "$tmp/warm.json" 2>/dev/null || fail "warm run $i exited non-zero"
    ms=$(( $(now_ms) - t0 )); walls+=("$ms")
    doc_check "$tmp/warm.json" || fail "warm run $i: document check"
  done
  say "warm runs (ms): ${walls[*]}"
  med=$(printf '%s\n' "${walls[@]}" | median)
  print_phases "$tmp/warm.json"
  t0=$(now_ms); "$bin" --json --no-progress --rescan > "$tmp/rescan.json" 2>/dev/null || fail "--rescan run exited non-zero"
  rescan_ms=$(( $(now_ms) - t0 ))
  say "median warm: ${med} ms (budget ${BUDGET_MS} ms, headroom $(( BUDGET_MS - med )) ms)"
  say "--rescan: ${rescan_ms} ms (budget ${RESCAN_BUDGET_MS} ms)"
  local bad=0
  if [ "$med" -gt "$BUDGET_MS" ]; then say "MISS: median ${med} ms > ${BUDGET_MS} ms"; print_phases "$tmp/warm.json"; bad=1; fi
  if [ "$rescan_ms" -gt "$RESCAN_BUDGET_MS" ]; then say "MISS: --rescan ${rescan_ms} ms > ${RESCAN_BUDGET_MS} ms"; bad=1; fi
  [ $bad = 0 ] || exit 1
  say "PASS (owner machine)"
}

# A fixture HOME under a sentinel root, as the e2e tests do, so nothing outside the temp dir is read or written.
fixture_env() {
  local root=$1
  mkdir -p "$root/home/.npm/_cacache" "$root/xdg/state" "$root/xdg/cache" "$root/xdg/config" "$root/bin"
  : > "$root/.dc-test-root"
  head -c 65536 /dev/zero > "$root/home/.npm/_cacache/blob"
  printf '#!/bin/sh\necho "%s/xdg/cache/go-build"\n' "$root" > "$root/bin/go"; chmod +x "$root/bin/go"
}

counters_mode() {
  local bin=$1 tmp=$2 root tree n a b cache
  root=$tmp/fx tree=$tmp/tree cache=$tmp/scan.cache
  fixture_env "$root"
  say "counters mode (DC_BENCH_OWNER_MACHINE unset): no wall-clock assertion"
  env -i PATH="$root/bin:/usr/bin:/bin" HOME="$root/home" XDG_STATE_HOME="$root/xdg/state" XDG_CACHE_HOME="$root/xdg/cache" \
    XDG_CONFIG_HOME="$root/xdg/config" DC_TEST_ROOT="$root" LANG=C "$bin" --json --no-progress > "$tmp/report.json" 2>/dev/null \
    || fail "report exited non-zero"
  doc_check "$tmp/report.json" || fail "report: document check"
  say "report document ok: $(jq '.items | length' "$tmp/report.json") items"
  mkdir -p "$tree"
  ( cd "$tree" && seq 0 $((OUTER - 1)) | xargs -I{} -n1 mkdir -p "{}" \
    && for a in $(seq 0 $((OUTER - 1))); do seq 0 $((INNER - 1)) | sed "s|^|$a/|"; done | xargs -n 500 mkdir -p )
  n=$(find "$tree" -type d | wc -l)
  sleep "$RACY_SLEEP" # the walker re-lists directories modified within the last 2 s ("racy git")
  "$bin" scan --backend walker --cache-file "$cache" --json "$tree" > "$tmp/s1.json" 2>/dev/null || fail "first scan exited non-zero"
  "$bin" scan --backend walker --cache-file "$cache" --json "$tree" > "$tmp/s2.json" 2>/dev/null || fail "second scan exited non-zero"
  a=$(jq -r '"\(.dirs_listed) \(.dirs_cached)"' "$tmp/s1.json"); b=$(jq -r '"\(.dirs_listed) \(.dirs_cached)"' "$tmp/s2.json")
  say "fixture directories: $n; first scan dirs_listed/dirs_cached: $a; second scan: $b"
  [ "${a%% *}" = "$n" ] || fail "first scan listed ${a%% *} directories, the fixture has $n"
  [ "${b%% *}" = 0 ] && [ "${b##* }" = "$n" ] || fail "second scan rescanned: dirs_listed/dirs_cached = $b, want 0 and $n (cache_hits must equal dirs_scanned)"
  say "PASS (counters)"
}

run() {
  local bin=$1
  [ -x "$bin" ] || { say "binary not found: $bin (cargo build --release)"; exit 64; }
  tmp=$(mktemp -d); trap 'rm -rf "${tmp:-}"' EXIT
  if [ "${DC_BENCH_OWNER_MACHINE:-0}" = 1 ]; then owner_mode "$bin" "$tmp"; else counters_mode "$bin" "$tmp"; fi
}

selftest() {
  local self bad=0 n=0 out rc
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "${d:-}"' EXIT
  # stub <mode>: `scan` honours --cache-file and --rescan; the report prints what the mode says.
  stub() {
    local mode=$1 path=$d/stub-$1
    cat > "$path" <<STUB
#!/bin/bash
mode=$mode
if [ "\${1:-}" = scan ]; then
  cache=""; rescan=0; tree=""
  shift
  while [ \$# -gt 0 ]; do case \$1 in --cache-file) cache=\$2; shift;; --rescan) rescan=1;; --backend) shift;; --json) ;; *) tree=\$1;; esac; shift; done
  if [ "\$tree" = "" ]; then echo '{"walk_ms":1,"total_ms":2,"dirs_listed":3,"dirs_cached":3}'; exit 0; fi
  n=\$(find "\$tree" -type d | wc -l)
  if [ "\$mode" != rescans ] && [ -n "\$cache" ] && [ -e "\$cache" ] && [ \$rescan = 0 ]; then echo "{\"dirs_listed\":0,\"dirs_cached\":\$n,\"walk_ms\":1,\"total_ms\":1}"
  else [ -n "\$cache" ] && : > "\$cache"; echo "{\"dirs_listed\":\$n,\"dirs_cached\":0,\"walk_ms\":1,\"total_ms\":1}"; fi
  exit 0
fi
echo x >> "$d/calls-$mode"
[ "\$mode" = slow ] && sleep 0.3
if [ "\$mode" = empty ]; then echo '{}'; exit 0; fi
echo '{"mode":"plan","exit_code":0,"items":[{"family":"caches","status":"would_free","path":"/h/.npm/_cacache"},{"family":"caches","status":"skipped","path":"/h/.cache/go-build"}]}'
STUB
    chmod +x "$path"; echo "$path"
  }
  expect() { # <want-rc> <name> <pattern> <cmd...>
    local want=$1 name=$2 pat=$3; shift 3
    out=$("$@" 2>&1); rc=$?
    if [ "$rc" = "$want" ] && echo "$out" | grep -q -- "$pat"; then n=$((n+1)); echo "ok: $name"; else echo "SELFTEST BAD: $name (rc=$rc, want $want, pattern '$pat'): $out"; bad=$((bad+1)); fi
  }
  export DC_BENCH_DIRS_OUTER=4 DC_BENCH_DIRS_INNER=5 DC_BENCH_RACY_SLEEP=0
  expect 0 "counters mode passes a binary that caches" "PASS (counters)" "$self" --bin "$(stub good)"
  expect 1 "a stub that prints {} instantly fails" "empty document" "$self" --bin "$(stub empty)"
  expect 1 "a stub that rescans every time fails the counters" "second scan rescanned" "$self" --bin "$(stub rescans)"
  echo 0.10 1 1 > "$d/load-low"; echo "99.50 40.0 30.0 1/1 1" > "$d/load-high"
  export DC_BENCH_OWNER_MACHINE=1
  expect 77 "load above the threshold is INCONCLUSIVE and exits 77" "INCONCLUSIVE: load" env DC_BENCH_LOADAVG_FILE="$d/load-high" DC_BENCH_MAX_LOAD=12 "$self" --bin "$(stub good)"
  : > "$d/calls-good"
  expect 0 "owner mode passes a fast binary" "PASS (owner machine)" env DC_BENCH_LOADAVG_FILE="$d/load-low" DC_BENCH_MAX_LOAD=12 "$self" --bin "$(stub good)"
  if [ "$(wc -l < "$d/calls-good")" = 9 ]; then n=$((n+1)); echo "ok: owner mode ran 1 cold + 7 warm + 1 rescan report runs"; else echo "SELFTEST BAD: report runs: $(wc -l < "$d/calls-good")"; bad=$((bad+1)); fi
  expect 1 "owner mode: a median above the budget is a MISS that prints the phases" "MISS: median" env DC_BENCH_LOADAVG_FILE="$d/load-low" DC_BENCH_MAX_LOAD=12 DC_BENCH_BUDGET_MS=100 "$self" --bin "$(stub slow)"
  expect 1 "owner mode: a {} stub fails too" "empty document" env DC_BENCH_LOADAVG_FILE="$d/load-low" DC_BENCH_MAX_LOAD=12 "$self" --bin "$(stub empty)"
  expect 1 "owner mode: a --rescan above its budget fails" "MISS: --rescan" env DC_BENCH_LOADAVG_FILE="$d/load-low" DC_BENCH_MAX_LOAD=12 DC_BENCH_RESCAN_BUDGET_MS=100 "$self" --bin "$(stub slow)"
  [ "$bad" = 0 ] || { echo "bench-dry-run selftest: $bad failed"; exit 1; }
  echo "bench-dry-run selftest: $n ok"
}

bin=${DC_BENCH_BIN:-${CARGO_TARGET_DIR:-target}/release/disk-cleanup}
# The default binary is rebuilt (a no-op when fresh): a gate runs this before its own release build.
build_default() { [ -n "${DC_BENCH_BIN:-}" ] || cargo build --release --locked -q || fail "cargo build --release failed"; }
case ${1:-} in
  --selftest) selftest ;;
  --bin) [ $# = 2 ] || { echo "usage: $0 [--bin PATH] | --selftest"; exit 64; }; run "$2" ;;
  "") build_default; run "$bin" ;;
  *) echo "usage: $0 [--bin PATH] | --selftest"; exit 64 ;;
esac

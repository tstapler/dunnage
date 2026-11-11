#!/usr/bin/env bash
# bench-jobs.sh [--bin PATH] [--dirs N] [--pkgs N] [--reps N] [--jobs "1 4 8 16"]   (Story 3.2.4, Task 3.2.4b)
# Times `disk-cleanup --apply --kinds build-dirs --jobs J` on a fixture of N node_modules trees that lives on a
# btrfs filesystem under "$HOME/.cache/dc-fixtures" (the fixture-location rule of Task 0.2.4a: the directory is
# made with `mktemp -d -p`, and only after `stat -f -c %T` says btrfs). Every run gets a fresh copy of one aged
# template, a dry run for the plan-id, and then the timed apply. Prints one row per (jobs, repetition) and the
# median per jobs value. Everything it deletes is a copy below the fixture root; it never reads a real HOME.
set -u -o pipefail

BIN=${DC_BENCH_BIN:-${CARGO_TARGET_DIR:-target}/release/disk-cleanup}
DIRS=16 PKGS=400 REPS=3 JOBS="1 4 8 16"
while [ $# -gt 0 ]; do
  case $1 in
    --bin) BIN=$2; shift ;; --dirs) DIRS=$2; shift ;; --pkgs) PKGS=$2; shift ;; --reps) REPS=$2; shift ;; --jobs) JOBS=$2; shift ;;
    *) echo "usage: $0 [--bin PATH] [--dirs N] [--pkgs N] [--reps N] [--jobs \"1 4 8 16\"]"; exit 64 ;;
  esac
  shift
done
[ -x "$BIN" ] || { echo "binary not found: $BIN"; exit 64; }
base=$HOME/.cache/dc-fixtures
mkdir -p "$base"
[ "$(stat -f -c %T "$base")" = btrfs ] || { echo "SKIP: $base is not on btrfs"; exit 77; }
root=$(mktemp -d -p "$base" bench-jobs-XXXXXX)
trap 'rm -rf "${root:?}"' EXIT
case $(realpath "$root") in "$(realpath "$base")"/bench-jobs-*) ;; *) echo "refusing: $root is outside $base"; exit 1 ;; esac

# Template: DIRS projects, each with PKGS packages of 10 small files (the shape of a node_modules), aged 40 days.
tpl=$root/template
for i in $(seq 1 "$DIRS"); do
  p=$tpl/code/p$i
  mkdir -p "$p/node_modules"
  echo '{}' > "$p/package.json"; echo "lock-$i" > "$p/package-lock.json"
  seq 1 "$PKGS" | sed "s|^|$p/node_modules/k|" | xargs -P8 -n 500 mkdir -p
  awk -v d="$p/node_modules" -v n="$PKGS" 'BEGIN { for (k = 1; k <= n; k++) for (f = 0; f < 10; f++) print d "/k" k "/f" f ".js" }' | xargs -P8 -n 500 touch
done
find "$tpl" -exec touch -h -d '40 days ago' {} +
files=$(find "$tpl/code" -type f | wc -l)
echo "fixture: $DIRS build dirs, $files files per run, on $(stat -f -c %T "$base"), $(nproc) CPUs, load $(cut -d' ' -f1 /proc/loadavg)"

now_ms() { echo $(( $(date +%s%N) / 1000000 )); }
declare -A all
for j in $JOBS; do
  for r in $(seq 1 "$REPS"); do
    run=$root/run-$j-$r
    mkdir -p "$run/home" "$run/xdg/state" "$run/xdg/cache" "$run/xdg/config"
    : > "$run/.dc-test-root"
    cp -a --reflink=auto "$tpl/code" "$run/home/code"
    sync -f "$run"
    env_=(env -i PATH="$PATH" HOME="$run/home" XDG_STATE_HOME="$run/xdg/state" XDG_CACHE_HOME="$run/xdg/cache" XDG_CONFIG_HOME="$run/xdg/config" DC_TEST_ROOT="$run" LANG=C)
    id=$("${env_[@]}" "$BIN" --kinds build-dirs --json --no-progress < /dev/null | jq -r .plan_id)
    t0=$(now_ms)
    "${env_[@]}" "$BIN" --apply --kinds build-dirs --plan-id "$id" --jobs "$j" --no-progress --json < /dev/null > "$run/apply.json" 2>/dev/null
    rc=$?
    ms=$(( $(now_ms) - t0 ))
    left=$(find "$run/home/code" -name node_modules | wc -l)
    ex=$(jq -r '.summary.timings.execute_ms // empty' "$run/apply.json")
    echo "jobs=$j rep=$r exit=$rc execute_ms=${ex:-n/a} wall_ms=$ms build_dirs_left=$left"
    all[$j]="${all[$j]:-} ${ex:-$ms}"
    rm -rf "$run"
  done
done
echo "median execute_ms per --jobs (wall_ms when the document has no timings):"
for j in $JOBS; do
  med=$(printf '%s\n' ${all[$j]} | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}')
  echo "  jobs=$j median=$med"
done

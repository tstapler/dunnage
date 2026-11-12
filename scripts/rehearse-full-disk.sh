#!/usr/bin/env bash
# shellcheck disable=SC2016  # the single-quoted bodies run in the child shell on purpose
# Owner rehearsal of the dry run -> plan-id -> apply flow with the state dir on a full filesystem
# (plan Story 2.3.3, Gate B checklist). Usage:
#   rehearse-full-disk.sh [--no-reserve | --prime 'CMD'] -- <command...>
#       run <command> with XDG_STATE_HOME on a full tmpfs. Default: a 1 MiB last_plan.reserve is written before
#       the fill (what the binary leaves behind). --prime 'CMD' runs CMD first with XDG_STATE_HOME set (for
#       example 'disk-cleanup status') so the real binary creates its own reserves. --no-reserve: nothing before
#       the fill (the first run on an already-full disk).
#   --cache-too (Story 6.5.6, Gate F): a second 24 MiB tmpfs, also full, is mounted for XDG_CACHE_HOME, so the
#       explorer's cache save has no room either (UX-T22). Combine with the flags above, before `--`.
#   rehearse-full-disk.sh --selftest   prove the fill, the reserve release, the cache mount and the private mount
# Fixture-only: the one filesystem it fills is a 24 MiB tmpfs mounted over a fresh mktemp directory inside a private
# mount namespace; nothing else is written, and the real disk and the real state directory are never touched
# (HOME and every other path are whatever <command> is given).
# The tmpfs is 24 MiB, not smaller: the tool creates a missing reserve only with 16 MiB available
# (RESERVE_RECREATE_MIN_AVAIL in crates/adapters/src/state.rs), so a smaller one could never hold its reserves.
# Needs `unshare -rm` (rootless user and mount namespace; the mount is private to the child and vanishes with it).
set -euo pipefail

fill_and_run() { # $1 = default|none|prime, $2 = prime command or '', rest = command; CACHE_TOO=1 adds the cache tmpfs
  local mode="$1" prime="$2"; shift 2
  local state cache=""
  export MODE="$mode" PRIME="$prime" CACHE_TOO="${CACHE_TOO:-0}"
  state="$(mktemp -d -p "${TMPDIR:-/tmp}" dc-rehearse.XXXXXX)"
  [ "$CACHE_TOO" = 1 ] && cache="$(mktemp -d -p "${TMPDIR:-/tmp}" dc-rehearse-cache.XXXXXX)"
  local rc=0 uid gid
  uid="$(id -u)"; gid="$(id -g)"
  # Outer namespace: root of its own user namespace, only to mount the tmpfs. The rest runs in a nested user
  # namespace that maps the caller's own uid back, so the rehearsed binary sees euid != 0 (as in the owner's
  # real session) and the tool's root rules do not change the flow being rehearsed.
  unshare -rm bash -c '
    set -euo pipefail
    state="$1"; uid="$2"; gid="$3"; cache="$4"; shift 4
    mount -t tmpfs -o size=24m tmpfs "$state"
    [ -z "$cache" ] || mount -t tmpfs -o size=24m tmpfs "$cache"
    exec unshare -U --map-user="$uid" --map-group="$gid" bash -c "
      set -euo pipefail
      mode=\"\$1\"; prime=\"\$2\"; state=\"\$3\"; cache=\"\$4\"; shift 4
      mkdir -p \"\$state/disk-cleanup\"
      export XDG_STATE_HOME=\"\$state\"
      case \"\$mode\" in
        default) dd if=/dev/zero of=\"\$state/disk-cleanup/last_plan.reserve\" bs=1k count=1024 status=none ;;
        prime)   sh -c \"\$prime\" || echo \"rehearsal: the prime command exited \$? (continuing: status exits 2 on a CRIT disk)\" >&2 ;;
        none)    : ;;
      esac
      dd if=/dev/zero of=\"\$state/fill\" bs=4k count=8000 status=none 2>/dev/null || true
      if [ -n \"\$cache\" ]; then
        mkdir -p \"\$cache/disk-cleanup\"
        export XDG_CACHE_HOME=\"\$cache\"
        dd if=/dev/zero of=\"\$cache/fill\" bs=4k count=8000 status=none 2>/dev/null || true
        if ( echo x > \"\$cache/disk-cleanup/new-file\" ) 2>/dev/null; then echo \"REHEARSAL BAD: the cache dir is not full\" >&2; exit 3; fi
        rm -f \"\$cache/disk-cleanup/new-file\"
        echo \"rehearsal: XDG_CACHE_HOME=\$cache is full\" >&2
      fi
      if ( echo x > \"\$state/disk-cleanup/new-file\" ) 2>/dev/null; then echo \"REHEARSAL BAD: the state dir is not full\" >&2; exit 3; fi
      rm -f \"\$state/disk-cleanup/new-file\"
      echo \"rehearsal: XDG_STATE_HOME=\$state is full (reserve: \$mode)\" >&2
      \"\$@\"
    " _ "$MODE" "$PRIME" "$state" "$cache" "$@"
  ' _ "$state" "$uid" "$gid" "$cache" "$@" || rc=$?
  rmdir "$state" 2>/dev/null || true  # empty mountpoint dirs only
  [ -z "$cache" ] || rmdir "$cache" 2>/dev/null || true
  return $rc
}

if [ "${1:-}" = --selftest ]; then
  bad=0
  ok() { echo "ok: $*"; }
  no() { echo "SELFTEST BAD: $*"; bad=1; }
  # 1. without a reserve: writing data fails, as on the first run on a full disk
  if fill_and_run none '' bash -c 'echo x > "$XDG_STATE_HOME/disk-cleanup/last_plan"' 2>/dev/null; then
    no "creating last_plan on the full tmpfs succeeded"; else ok "no reserve: creating last_plan fails (ENOSPC)"; fi
  # 2. with the default 1 MiB reserve: truncating the reserve frees room and the retry succeeds
  if fill_and_run default '' bash -c '
      d="$XDG_STATE_HOME/disk-cleanup"
      ( echo x > "$d/last_plan" ) 2>/dev/null && exit 4   # first write must fail
      : > "$d/last_plan.reserve"                          # release the reserve
      echo saved > "$d/last_plan"                         # retry must succeed
      [ "$(cat "$d/last_plan")" = saved ]' 2>/dev/null; then
    ok "with a reserve: first write fails, release + retry succeeds"; else no "reserve release did not make room"; fi
  # 3. --prime runs before the fill, so a binary can create its own reserves
  if fill_and_run prime 'dd if=/dev/zero of="$XDG_STATE_HOME/disk-cleanup/audit.reserve" bs=1k count=64 status=none' \
      bash -c 'test -s "$XDG_STATE_HOME/disk-cleanup/audit.reserve"' 2>/dev/null; then
    ok "--prime output exists on the full state dir"; else no "--prime did not run before the fill"; fi
  # 3b. a prime command that exits non-zero (`disk-cleanup status` exits 2 on a CRIT disk) does not abort the rehearsal
  if fill_and_run prime 'dd if=/dev/zero of="$XDG_STATE_HOME/disk-cleanup/audit.reserve" bs=1k count=8 status=none; exit 2' \
      bash -c 'test -s "$XDG_STATE_HOME/disk-cleanup/audit.reserve"' 2>/dev/null; then
    ok "a failing --prime command still leaves the rehearsal running"; else no "a failing --prime command aborted the rehearsal"; fi
  # 3c. the exit status of the rehearsed command is the script's
  fill_and_run none '' bash -c 'exit 7' 2>/dev/null && rc7=0 || rc7=$?
  if [ "$rc7" = 7 ]; then ok "the command's exit status is passed through"; else no "exit status $rc7, want 7"; fi
  # 3d. the rehearsed command runs as the caller's own uid, not as the namespace's root
  if fill_and_run none '' bash -c 'test "$(id -u)" = "'"$(id -u)"'"' 2>/dev/null; then
    ok "the command runs as the caller's uid (not root)"; else no "the command ran as another uid"; fi
  # 3e. --cache-too: XDG_CACHE_HOME is a second full tmpfs; without it the variable is the caller's own
  if CACHE_TOO=1 fill_and_run none '' bash -c '
      case "$XDG_CACHE_HOME" in *dc-rehearse-cache*) ;; *) exit 5 ;; esac
      ( echo x > "$XDG_CACHE_HOME/disk-cleanup/f" ) 2>/dev/null && exit 6
      [ "$(stat -f -c %T "$XDG_CACHE_HOME")" = tmpfs ] && [ "$XDG_CACHE_HOME" != "$XDG_STATE_HOME" ]' 2>/dev/null; then
    ok "--cache-too: XDG_CACHE_HOME is its own full tmpfs"; else no "--cache-too did not give a full cache tmpfs"; fi
  if fill_and_run none '' bash -c 'case "${XDG_CACHE_HOME:-}" in *dc-rehearse-cache*) exit 7 ;; esac' 2>/dev/null; then
    ok "without --cache-too the cache directory is left alone"; else no "the cache tmpfs was mounted without --cache-too"; fi
  # 4. the namespace is private: nothing leaks to the host
  if mount | grep -q dc-rehearse; then no "tmpfs leaked into the host mount table"; else ok "mount is private to the child"; fi
  [ "$bad" = 0 ] && { echo "selftest: all ok"; exit 0; } || exit 1
fi

mode=default; prime=''
while :; do
  case "${1:-}" in
    --no-reserve) mode=none; shift ;;
    --prime) mode=prime; prime="${2:?--prime needs a command}"; shift 2 ;;
    --cache-too) export CACHE_TOO=1; shift ;;
    *) break ;;
  esac
done
[ "${1:-}" = -- ] && shift
[ $# -gt 0 ] || { echo "usage: $0 [--no-reserve | --prime 'CMD'] [--cache-too] -- <command...> | --selftest" >&2; exit 64; }
fill_and_run "$mode" "$prime" "$@"

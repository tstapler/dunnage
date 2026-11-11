#!/usr/bin/env bash
# --workspace_status_command: the values crates/cli/build.rs bakes under Cargo (Story 2.2.1d), computed with the
# same git commands. Outside a git checkout every value is unknown/false.
git rev-parse --show-toplevel >/dev/null 2>&1 || {
  printf 'STABLE_DC_GIT_COMMIT unknown\nSTABLE_DC_GIT_DIRTY false\nSTABLE_DC_SCAN_REV unknown\nSTABLE_DC_SCAN_DIRTY false\n'; exit 0; }
commit=$(git rev-parse HEAD 2>/dev/null || echo unknown)
dirty=false; [ -n "$(git status --porcelain 2>/dev/null)" ] && dirty=true
scan_rev=$(git log -1 --format=%H -- crates/scan 2>/dev/null); : "${scan_rev:=$commit}"
scan_dirty=false; [ -n "$(git status --porcelain -- crates/scan 2>/dev/null)" ] && scan_dirty=true
printf 'STABLE_DC_GIT_COMMIT %s\nSTABLE_DC_GIT_DIRTY %s\nSTABLE_DC_SCAN_REV %s\nSTABLE_DC_SCAN_DIRTY %s\n' \
  "$commit" "$dirty" "$scan_rev" "$scan_dirty"

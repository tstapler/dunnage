#!/usr/bin/env bash
# fixture-manifest.sh <fixture-root> : the fixture's own checksum list (relative path, type, mode, mtime for every
# entry; sha256 for readable files; symlink targets). Directory sizes vary by filesystem and are left out.
# repoB/private is mode 000 on purpose, so find exits 1 on it; that is expected and its stderr is dropped.
set -uo pipefail
cd "$1" || exit 2
export LC_ALL=C
{ find . -mindepth 1 -printf '%P\t%y\t%m\t%T@\n' 2>/dev/null; true; } | sort
# the worktree pointer embeds the fixture's absolute path, so only its size is listed
{ find . -type f -readable ! -path ./repoA-wt1/.git -print0 2>/dev/null; true; } | sort -z | xargs -0 -r sha256sum
printf 'size repoA-wt1/.git %s\n' "$(stat -c %s repoA-wt1/.git)"
{ find . -type l -printf 'link %P -> %l\n' 2>/dev/null; true; } | sort

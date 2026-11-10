#!/usr/bin/env bash
# check-readonly.sh [repo-root] : metascope stays read-only. Fails if non-test code of any crate calls a delete primitive.
# Everything from the first `#[cfg(test)]` line of a file on is treated as test code.
set -u
root=${1:-.}; hits=0
while IFS= read -r f; do
  out=$(awk '/#\[cfg\(test\)\]/ {exit} /remove_file|remove_dir|unlinkat|libc::unlink|fs::remove/ {print FILENAME ":" FNR ": " $0}' "$f")
  [ -n "$out" ] && { echo "$out"; hits=$((hits+1)); }
done < <(find "$root/crates" -name '*.rs' -not -path '*/tests/*' -not -path '*/target/*')
[ $hits = 0 ] && echo "ok: no delete primitive in non-test code" || echo "FAIL: $hits file(s) call a delete primitive outside tests"
[ $hits = 0 ]

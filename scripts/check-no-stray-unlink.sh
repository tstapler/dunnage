#!/usr/bin/env bash
# shellcheck disable=SC2154
# check-no-stray-unlink.sh [repo-root] | --selftest
# ADR-003 decision 3 (Story 2.1.4): only the delete engine unlinks, and `git worktree remove` has one door.
#  1. Unlink leg: non-test code under crates/*/src (and crates/*/build.rs) must not name remove_file, remove_dir,
#     remove_dir_all, unlink, unlinkat or rmdir, except in the files in UNLINK_OK: delete.rs (the engine), dirops.rs
#     (the `DirOps` trait and its one `rustix::fs::unlinkat`) and testing.rs (the fault wrapper forwarding that call).
#     Also allowed: one line marked `readonly-ok(cache-temp)` in crates/scan/src/cache.rs (the scan cache's own temp
#     file, policed by check-scan-readonly.sh) and the LEGACY_OK file below.
#  2. Worktree leg: outside crates/adapters/src/command.rs and crates/core/src/ports.rs, the non-comment text of a
#     file (joined, so line breaks do not hide it) must not hold "worktree" followed within 120 characters by
#     "remove" as string literals, nor the words `worktree remove` in one string. A text search is the second line;
#     the first is the runtime classifier (Story 1.1.2).
# Everything from the first `#[cfg(test)]` line of a file on is test code. A missing crates/ or a tree with no
# .rs file exits 2: a check that scans nothing must not pass. Exit 1 names file and line.
set -u -o pipefail

UNLINK_OK='^crates/adapters/src/(delete|dirops|testing)\.rs$'
WORKTREE_OK='^crates/(adapters/src/command|core/src/ports)\.rs$'
# Pre-existing `std::fs::remove_dir_all` and `git worktree remove --force` in the legacy cleaner in crates/cli/src/main.rs
# (before the plan's CLI); both must go through the engine once that code is rewired. Listed here, not silent.
LEGACY_OK='^crates/cli/src/main\.rs$'

scan_file() {  # $1 = repo-relative path, $2 = absolute path; prints "path:line: leg: text"
  UNLINK_OK="$UNLINK_OK" WORKTREE_OK="$WORKTREE_OK" LEGACY_OK="$LEGACY_OK" REL="$1" perl -0777 -ne '
    my $rel = $ENV{REL};
    my $unlink_ok = $rel =~ /$ENV{UNLINK_OK}/ || $rel =~ /$ENV{LEGACY_OK}/;
    my $wt_ok = $rel =~ /$ENV{WORKTREE_OK}/ || $rel =~ /$ENV{LEGACY_OK}/;
    my @raw = split /\n/, $_, -1;
    my ($text, @marked) = ("");
    for my $i (0 .. $#raw) {
      last if $raw[$i] =~ /#\[cfg\(test\)\]/;
      my $l = $raw[$i];
      $marked[$i] = 1 if $rel eq "crates/scan/src/cache.rs" && $l =~ /readonly-ok\(cache-temp\)/;
      $l =~ s{(^|\s)//.*$}{$1};
      $text .= $l . "\n";
    }
    my @lines = split /\n/, $text, -1;
    if (!$unlink_ok) {
      for my $i (0 .. $#lines) {
        next if $marked[$i];
        print "$rel:" . ($i + 1) . ": unlink: $lines[$i]\n"
          if $lines[$i] =~ /\b(remove_(file|dir_all|dir)|unlink(?:at)?|rmdir)\b/;
      }
    }
    if (!$wt_ok) {
      while ($text =~ /("worktree"[^;]{0,120}?"remove"|\bworktree\s+remove\b)/gs) {
        my $line = 1 + (() = substr($text, 0, $-[0]) =~ /\n/g);
        print "$rel:$line: worktree-remove: " . ($lines[$line - 1] // "") . "\n";
      }
    }
  ' "$2"
}

check() {
  local root=${1:-.} files=0 hits=0 f rel out
  [ -d "$root/crates" ] || { echo "check-no-stray-unlink: $root/crates not found" >&2; return 2; }
  while IFS= read -r f; do
    files=$((files+1)); rel=${f#"$root"/}
    out=$(scan_file "$rel" "$f") || { echo "check-no-stray-unlink: scanner failed on $rel" >&2; return 2; }
    [ -n "$out" ] && { echo "$out"; hits=$((hits+1)); }
  done < <(find "$root"/crates -type f \( -path '*/src/*.rs' -o -name build.rs \) -not -path '*/tests/*' -not -path '*/target/*' | sort)
  [ "$files" -gt 0 ] || { echo "check-no-stray-unlink: no .rs files under $root/crates" >&2; return 2; }
  if [ "$hits" = 0 ]; then echo "ok: no stray unlink and no worktree-remove spelling outside the allowed files ($files files; legacy exemption: crates/cli/src/main.rs)"
  else echo "FAIL: $hits file(s) name an unlink primitive or a worktree removal outside the allowed files"; fi
  [ "$hits" = 0 ]
}

selftest() {
  local self bad=0 n=0 out rc d
  command -v perl >/dev/null || { echo "check-no-stray-unlink: perl not found" >&2; exit 2; }
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "${d:-}"' EXIT INT TERM
  expect() { local want=$1 name=$2 pat=$3; shift 3; out=$("$@" 2>&1); rc=$?
    if [ "$rc" = "$want" ] && grep -q -- "$pat" <<<"$out"; then n=$((n+1)); echo "ok: $name"; else echo "SELFTEST BAD: $name (rc=$rc, want $want): $out"; bad=$((bad+1)); fi; }
  local r=$d/r a=$d/r/crates/adapters/src c=$d/r/crates/core/src
  mkdir -p "$a" "$c" "$r/crates/scan/src"
  printf 'pub fn f() {}\n' > "$c/lib.rs"
  expect 0 "the clean tree exits 0" "^ok:" "$self" "$r"
  printf 'fn a() {\n    let _ = std::fs::remove_dir_all("x");\n}\n' > "$c/stray.rs"
  expect 1 "stray_unlink_should_exit_1_naming_file_line_When_remove_dir_all_outside_delete_rs" "crates/core/src/stray.rs:2:" "$self" "$r"
  rm "$c/stray.rs"
  for f in "remove_file(p)" "remove_dir(p)" "unlinkat(fd, n, f)" "libc::unlink(p)" "libc::rmdir(p)" "use std::fs::{remove_dir_all, read}"; do
    printf 'fn a() {\n    let _ = %s;\n}\n' "$f" > "$c/stray.rs"
    expect 1 "a planted $f is caught" "stray.rs:2:" "$self" "$r"
  done
  rm "$c/stray.rs"
  printf 'fn a() { let _ = rustix::fs::unlinkat(); remove_dir_all(); }\n' > "$a/delete.rs"
  expect 0 "delete.rs may unlink" "^ok:" "$self" "$r"
  rm "$a/delete.rs"
  printf '// std::fs::remove_dir_all(x)\n/// remove_file\nfn a() { b(); } // unlinkat\n' > "$c/doc.rs"
  expect 0 "a comment naming a primitive is ignored" "^ok:" "$self" "$r"
  printf 'fn a() {}\n#[cfg(test)]\nmod t { fn g() { std::fs::remove_dir_all("x").ok(); } }\n' > "$c/doc.rs"
  expect 0 "a primitive after #[cfg(test)] is ignored" "^ok:" "$self" "$r"
  printf 'fn a() { let _ = std::fs::remove_file("x"); }\n' > "$r/crates/scan/src/walker.rs"
  expect 1 "an unmarked remove_file in the scan crate fails" "walker.rs:1:" "$self" "$r"
  rm "$r/crates/scan/src/walker.rs"
  printf 'fn a() { let _ = std::fs::remove_file("x"); } // readonly-ok(cache-temp)\n' > "$r/crates/scan/src/cache.rs"
  expect 0 "the marked line in scan/cache.rs passes" "^ok:" "$self" "$r"
  printf 'fn a() { let _ = std::fs::remove_file("x"); } // readonly-ok(cache-temp)\n' > "$r/crates/scan/src/walker.rs"
  expect 1 "the marker in another file fails" "walker.rs:1:" "$self" "$r"
  rm "$r/crates/scan/src/walker.rs" "$r/crates/scan/src/cache.rs"
  # worktree forms, each outside the two allowed files
  printf 'fn a(p: P) { let _ = Cmd { args: vec!["worktree", "remove", p] }; }\n' > "$c/w.rs"
  expect 1 "worktree_remove_grep_should_exit_1_for_argv_array_split_lines_command_builder_and_format (vec form)" "w.rs:1:" "$self" "$r"
  printf 'fn a() {\n    let _ = &["worktree",\n        "remove"];\n}\n' > "$c/w.rs"
  expect 1 "the argv array split across lines fails" "w.rs:2:" "$self" "$r"
  printf 'fn a() {\n    Command::new("git").arg("worktree")\n        .arg("remove");\n}\n' > "$c/w.rs"
  expect 1 "the Command builder form fails" "w.rs:2:" "$self" "$r"
  printf 'fn a(p: P) -> String { format!("git worktree remove {p}") }\n' > "$c/w.rs"
  expect 1 "the format! form fails" "w.rs:1:" "$self" "$r"
  printf 'fn a() { let _ = &["worktree", "list"]; let _ = "removed"; }\n' > "$c/w.rs"
  expect 0 "worktree list without a nearby remove passes" "^ok:" "$self" "$r"
  printf '// vec!["worktree", "remove"]\nfn a() {}\n' > "$c/w.rs"
  expect 0 "a comment spelling the argv is ignored" "^ok:" "$self" "$r"
  rm "$c/w.rs"
  for f in "$a/command.rs" "$c/ports.rs"; do
    printf 'fn a(p: P) { let _ = vec!["worktree", "remove", p]; let _ = format!("git worktree remove {p}"); }\n' > "$f"
    expect 0 "the same forms inside $(basename "$f") pass" "^ok:" "$self" "$r"
    rm "$f"
  done
  mkdir -p "$r/crates/cli/src"
  printf 'fn a() { let _ = std::fs::remove_dir_all("x"); }\n' > "$r/crates/cli/src/main.rs"
  printf 'fn a() { let _ = std::fs::remove_dir_all("x"); let _ = &["worktree", "remove", "--force"]; }\n' > "$r/crates/cli/src/main.rs"
  expect 0 "the legacy cli exemption passes" "^ok:" "$self" "$r"
  rm "$r/crates/cli/src/main.rs"; printf 'fn a() { let _ = std::fs::remove_dir_all("x"); }\n' > "$r/crates/cli/src/other.rs"
  expect 1 "another cli file does not share the exemption" "other.rs:1:" "$self" "$r"
  rm "$r/crates/cli/src/other.rs"; printf 'fn main() { std::fs::remove_dir_all("x").ok(); }\n' > "$r/crates/core/build.rs"
  expect 1 "a build.rs is scanned" "build.rs:1:" "$self" "$r"
  rm -r "$r/crates"; mkdir -p "$r/crates/x/src"
  expect 2 "a tree with no .rs file fails closed" "no .rs files" "$self" "$r"
  expect 2 "a missing crates dir fails closed" "not found" "$self" "$d/none"
  echo "selftest: $n ok, $bad bad"; [ "$bad" = 0 ]
}

if [ "${1:-}" = --selftest ]; then selftest; else check "${1:-.}"; fi

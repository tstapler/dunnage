#!/usr/bin/env bash
# check-tui-deps.sh [repo-root] | --selftest
# Containment of crates/tui (ADR-005, Story 6.5.1), the sibling of check-core-deps.sh. Two halves:
#   manifest: the direct dependencies of dunnage-tui stay within an allowlist, and the crate has no build.rs,
#             no [build-dependencies] and no target whose source path leaves the crate;
#   source:   crates/tui/src (production code only; tests and examples may spawn processes) holds none of the
#             forbidden path segments and constructs. An allow-list of constructs, not a deny-list of function
#             names, so renaming the callee cannot evade it. Comments are stripped first; a hit prints
#             file:line: rule: text.
# shellcheck disable=SC2016
set -u -o pipefail
allowed_deps='dunnage-core ratatui crossterm unicode-width'
allowed_dev_deps='insta proptest expectrl dunnage-adapters'

manifest_half() { # $1 = root
  local root=$1 meta pkg bad=0 line kind name dir
  meta=$(cd "$root" && cargo metadata --no-deps --offline --format-version 1) || { echo "check-tui-deps: cargo metadata failed" >&2; return 2; }
  pkg=$(jq -c '.packages[] | select(.name == "dunnage-tui")' <<<"$meta")
  [ -n "$pkg" ] || { echo "FAIL: no workspace member named dunnage-tui"; return 1; }
  while read -r kind name; do
    case $kind in
      build) echo "FAIL: dunnage-tui has a [build-dependencies] table (build dependency $name)"; bad=1 ;;
      dev) grep -qwF -- "$name" <<<"$allowed_deps $allowed_dev_deps" || { echo "FAIL: dunnage-tui dev-depends on $name, which is not on the allowlist"; bad=1; } ;;
      *) grep -qwF -- "$name" <<<"$allowed_deps" || { echo "FAIL: dunnage-tui depends directly on $name, which is not on the allowlist ($allowed_deps)"; bad=1; } ;;
    esac
  done < <(jq -r '.dependencies[] | "\(.kind // "normal") \(.name)"' <<<"$pkg")
  dir=$(realpath -m -- "$(dirname "$(jq -r '.manifest_path' <<<"$pkg")")")
  if jq -e '.targets[] | select(.kind | index("custom-build"))' <<<"$pkg" >/dev/null || [ -e "$dir/build.rs" ]; then
    echo "FAIL: dunnage-tui has a build script (build.rs)"; bad=1
  fi
  while read -r line; do
    [ -n "$line" ] || continue
    line=$(realpath -m -- "$line")
    case $line in "$dir"/*) ;; *) echo "FAIL: dunnage-tui has a target whose path leaves the crate: $line"; bad=1 ;; esac
  done < <(jq -r '.targets[].src_path' <<<"$pkg")
  return $bad
}

# scan_src <dir>: one "file:line: rule: text" per hit.
scan_src() {
  find "$1" -name '*.rs' -print0 | sort -z | while IFS= read -r -d '' f; do
    perl -0777 -e '
      my ($f, $allow_io) = @ARGV; local $/; open(F, "<", $f) or die; my $t = <F>; close F;
      $t =~ s{/\*.*?\*/}{ my $x = $&; $x =~ s/[^\n]/ /g; $x }gse;
      $t =~ s{//[^\n]*}{}g;
      my @rules = (
        [seg   => qr/(?<![A-Za-z0-9_])std\s*::\s*(?:fs|process|os|net|env)\b/],
        [use   => qr/\buse\s+(?:::)?std\s*::\s*\{[^;]*?\b(?:fs|process|os|net|env)\b[^;]*;/s],
        [alias => qr/\buse\s+(?:::)?std\s+as\b|\bextern\s+crate\b/],
        [bare  => qr/(?<![A-Za-z0-9_])(?:fs|process|os|net|env)\s*::/],
        [deny  => qr/\b(?:rustix|libc|nix|ApplyToken|AllowedTarget|delete_tree|DirOps|CommandRunner)\b/],
        [meta  => qr/\b(?:include|include_str|include_bytes|macro_rules|asm|global_asm|paste)\s*!|#\s*!?\[\s*(?:path|link|link_name)\b|\bunsafe\b/],
        [ui    => qr/\bratatui\s*::\s*(?:init|restore)\b|\bterminal\s*::\s*size\b|\buse\s+crossterm[^;]*\bsize\b[^;]*;|\b(?:println|eprintln|print|eprint|dbg)\s*!/s],
      );
      push @rules, [stdio => qr/\bstd(?:out|err|in)\b/] unless $allow_io;
      for my $r (@rules) {
        while ($t =~ /$r->[1]/g) {
          my $pre = substr($t, 0, $-[0]); my $ln = 1 + ($pre =~ tr/\n//);
          my $m = $&; $m =~ s/\s+/ /g;
          print "$f:$ln: $r->[0]: " . substr($m, 0, 70) . "\n";
        }
      }
    ' "$f" "$([ "$(basename "$f")" = terminal.rs ] && echo 1)"
  done
}

check() {
  local root=${1:-.} bad=0 hits rc
  command -v jq >/dev/null || { echo "check-tui-deps: jq is required" >&2; return 2; }
  command -v perl >/dev/null || { echo "check-tui-deps: perl is required" >&2; return 2; }
  [ -d "$root/crates/tui/src" ] || { echo "FAIL: crates/tui/src does not exist"; return 1; }
  manifest_half "$root"; rc=$?
  [ $rc = 2 ] && return 2
  [ $rc = 0 ] || bad=1
  hits=$(cd "$root" && scan_src crates/tui/src)
  if [ -n "$hits" ]; then echo "FAIL: forbidden construct in crates/tui/src:"; echo "$hits"; bad=1; fi
  [ $bad = 0 ] && echo "ok: dunnage-tui keeps to its dependency allowlist and its source holds no forbidden construct"
  return $bad
}

selftest() {
  local self bad=0 n=0 out rc d f
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "${d:-}"' EXIT INT TERM
  expect() { local want=$1 name=$2 pat=$3; shift 3; out=$("$@" 2>&1); rc=$?
    if [ "$rc" = "$want" ] && grep -q -- "$pat" <<<"$out"; then n=$((n+1)); echo "ok: $name"; else echo "SELFTEST BAD: $name (rc=$rc): $out"; bad=$((bad+1)); fi; }
  fixture() { # $1 = dir: a workspace whose crates/tui is clean
    mkdir -p "$1/crates/tui/src"
    printf '[workspace]\nresolver = "2"\nmembers = ["crates/tui"]\n' > "$1/Cargo.toml"
    printf '[package]\nname = "dunnage-tui"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nratatui = "0.30"\ncrossterm = "0.29"\n' > "$1/crates/tui/Cargo.toml"
    printf '// std::fs::remove_file in a comment is fine\n/* use std::process::Command; */\nuse std::io::Write;\nuse std::sync::mpsc;\nuse std::time::Duration;\nfn f(w: &mut impl Write) { let _ = Duration::from_millis(5); crossterm::terminal::window_size(); }\n' > "$1/crates/tui/src/lib.rs"
    printf 'use std::io::{self, stdout};\nfn enter() { let _ = stdout(); }\n' > "$1/crates/tui/src/terminal.rs"
  }
  fixture "$d/ok"
  expect 0 "clean fixture passes (comments, window_size and stdout in terminal.rs are fine)" "^ok:" "$self" "$d/ok"
  rm -rf "$d/none"; mkdir -p "$d/none"
  expect 1 "a missing crates/tui fails closed" "crates/tui/src does not exist" "$self" "$d/none"

  # Manifest controls
  mfx() { # $1 = name, $2 = want-pattern, $3 = text appended to the manifest, $4 = optional extra file to create
    rm -rf "$d/m"; fixture "$d/m"; printf '%b' "$3" >> "$d/m/crates/tui/Cargo.toml"
    [ -n "${4:-}" ] && printf 'fn main() {}\n' > "$d/m/$4"
    expect 1 "$1" "$2" "$self" "$d/m"; }
  mfx "a libc dependency fails naming it" "depends directly on libc" 'libc = "0.2"\n'
  mfx "a rustix dependency fails naming it" "depends directly on rustix" 'rustix = "1"\n'
  mfx "a dunnage-adapters dependency fails naming it" "depends directly on dunnage-adapters" 'dunnage-adapters = "0.1"\n'
  mfx "a tokio dependency fails naming it" "depends directly on tokio" 'tokio = "1"\n'
  mfx "a nix dev-dependency off the allowlist fails" "dev-depends on nix" '\n[dev-dependencies]\nnix = "0.29"\n'
  mfx "a [build-dependencies] table fails" "build dependency cc" '\n[build-dependencies]\ncc = "1"\n'
  mfx "a build.rs fails" "has a build script" '' crates/tui/build.rs
  mfx "a [lib] path that leaves the crate fails" "leaves the crate" '\n[lib]\npath = "../core/src/lib.rs"\n'
  mfx "a [[bin]] path that leaves the crate fails" "leaves the crate" '\n[[bin]]\nname = "x"\npath = "../cli/src/main.rs"\n'
  rm -rf "$d/m"; fixture "$d/m"; printf '\n[dev-dependencies]\ninsta = "1"\nproptest = "1"\nexpectrl = "0.7"\ndunnage-adapters = "0.1"\n' >> "$d/m/crates/tui/Cargo.toml"
  expect 0 "the allowed dev-dependencies pass (adapters only for the pty probe example)" "^ok:" "$self" "$d/m"
  printf '\n[dependencies]\n[dependencies]\n' >> "$d/m/crates/tui/Cargo.toml"
  expect 2 "an unparsable manifest fails closed instead of passing" "cargo metadata failed" "$self" "$d/m"

  # Source controls: the 18 evasions of tui/tools/tui_src_scan.sh and two clean files
  rm -rf "$d/s"; mkdir -p "$d/s"
  w() { printf '%s\n' "$2" > "$d/s/$1.rs"; }
  w 01_import_fs 'use std::fs; fn f(){ fs::remove_dir("a"); fs::rename("a","b"); }'
  w 02_macro_path 'macro_rules! f { ($n:ident,$p:expr) => { std::fs::$n($p) } }'
  w 03_brace_process 'use std::process::{self, Command}; fn f(){ Command::new("rm"); }'
  w 04_brace_fs 'use std::{fs, io};'
  w 05_multiline 'use std::{
    io,
    process::Command,
};'
  w 06_leading_colons 'fn f(){ ::std::fs::write("a","b"); }'
  w 07_split_path 'fn f(){ std
  ::fs::remove_file("a"); }'
  w 08_nested_brace 'use std::{io::{self, Write}, fs};'
  w 09_alias 'use std as s; fn f(){ s::fs::remove_dir_all("a"); }'
  w 10_libc 'fn f(){ unsafe { libc::unlink(p) } }'
  w 11_include 'include!("../../adapters/src/delete.rs");'
  w 12_path_attr '#[path = "../../adapters/src/delete.rs"] mod d;'
  w 13_env 'fn f(){ let _ = std::env::var("X"); }'
  w 14_size_import 'use crossterm::terminal::{self, size};'
  w 15_println 'fn f(){ println!("x"); }'
  w 16_stdout_outside_terminal 'fn f(){ let _ = std::io::stdout(); }'
  w 17_token 'fn f(t: ApplyToken){}'
  w 18_extern 'extern crate std as s;'
  w 19_unsafe 'fn f(){ unsafe { core::hint::unreachable_unchecked() } }'
  w 20_dirops 'fn f(d: &dyn DirOps){}'
  w 21_terminal_size 'fn f(){ let _ = crossterm::terminal::size(); }'
  w 22_ratatui_init 'fn f(){ let _t = ratatui::init(); }'
  for f in "$d"/s/*.rs; do
    rm -rf "$d/one"; fixture "$d/one"; cp "$f" "$d/one/crates/tui/src/evade.rs"
    expect 1 "evasion $(basename "$f" .rs) is caught with file and line" "crates/tui/src/evade.rs:[0-9]" "$self" "$d/one"
  done
  rm -rf "$d/two"; fixture "$d/two"; w 16_stdout_inside_terminal_is_fine 'use std::io::stdout; fn f(){ let _ = stdout(); }'
  cp "$d/s/16_stdout_inside_terminal_is_fine.rs" "$d/two/crates/tui/src/terminal.rs"
  expect 0 "stdout inside terminal.rs is allowed" "^ok:" "$self" "$d/two"
  echo "selftest: $n ok, $bad bad"; [ $bad = 0 ]
}

if [ "${1:-}" = --selftest ]; then selftest; else check "${1:-.}"; fi

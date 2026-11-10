#!/usr/bin/env bash
# check-targets.sh [repo-root] | --selftest
# Compile-only early warning for the three release targets (Story 0.1.5): `cargo check --workspace --all-targets`
# for x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu and aarch64-apple-darwin. It never links, never runs a
# test and says nothing about runtime behaviour on those targets (the hosted-runner tests of Story 5.3.2 do that).
# A target missing from rustup is installed once with `rustup target add`; if that fails, or rustup/cargo is
# missing, the script exits 2 and never 0. Exit 1 means a target failed to compile.
# Toolchain recipe ("Implementer environment"): rustup's rustc must come first on PATH (Homebrew's rustc has no
# foreign std) and the sccache wrapper is turned off.
set -u -o pipefail
TARGETS=(x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu aarch64-apple-darwin)

check() {
  local root=${1:-.} t bad=0 rc
  local cargo=${DC_TARGETS_CARGO:-cargo} rustup=${DC_TARGETS_RUSTUP:-rustup}
  if [ -z "${DC_TARGETS_CARGO:-}" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
  export RUSTC_WRAPPER=
  command -v "$rustup" >/dev/null 2>&1 || { echo "check-targets: rustup not found on PATH (cannot install or verify targets)" >&2; return 2; }
  command -v "$cargo" >/dev/null 2>&1 || { echo "check-targets: cargo not found on PATH" >&2; return 2; }
  echo "check-targets: COMPILE-ONLY (cargo check; no linking, no tests, no runtime behaviour on these targets)"
  for t in "${TARGETS[@]}"; do
    if ! "$rustup" target list --installed 2>/dev/null | grep -qx "$t"; then
      echo "check-targets: installing missing rustup target $t"
      "$rustup" target add "$t" >&2 || { echo "FAIL: could not install rustup target $t" >&2; return 2; }
      "$rustup" target list --installed 2>/dev/null | grep -qx "$t" || { echo "FAIL: rustup target $t still not installed after 'rustup target add'" >&2; return 2; }
    fi
  done
  for t in "${TARGETS[@]}"; do
    ( cd "$root" && "$cargo" check --workspace --all-targets --target "$t" --message-format short ) >&2
    rc=$?
    if [ $rc = 0 ]; then echo "ok: $t (compile-only)"; else echo "FAIL: $t did not compile (cargo check exit $rc)"; bad=1; fi
  done
  return $bad
}

selftest() {
  d=""
  local self bad=0 n=0 out rc
  self=$(readlink -f "$0"); d=$(mktemp -d); trap 'rm -rf "${d:-}"' EXIT INT TERM
  expect() { local want=$1 name=$2 pat=$3; shift 3; out=$("$@" 2>&1); rc=$?
    if [ "$rc" = "$want" ] && grep -q -- "$pat" <<<"$out"; then n=$((n+1)); echo "ok: $name"; else echo "SELFTEST BAD: $name (rc=$rc): $out"; bad=$((bad+1)); fi; }
  mkdir -p "$d/bin" "$d/r"
  # stub rustup: lists the targets in $d/installed; `target add` appends unless $d/addfails exists
  cat > "$d/bin/rustup" <<STUB
#!/usr/bin/env bash
case "\$*" in
  "target list --installed") cat "$d/installed" ;;
  "target add "*) [ -e "$d/addfails" ] && exit 1; echo "\${@:3}" >> "$d/installed" ;;
esac
STUB
  # stub cargo: fails when the --target argument equals \$d/failtarget
  cat > "$d/bin/cargo" <<STUB
#!/usr/bin/env bash
[ "\$1" = check ] || exit 99
while [ \$# -gt 0 ]; do [ "\$1" = --target ] && t=\$2; shift; done
[ -e "$d/failtarget" ] && [ "\$t" = "\$(cat "$d/failtarget")" ] && { echo "error: stub compile failure for \$t"; exit 101; }
exit 0
STUB
  chmod +x "$d/bin/rustup" "$d/bin/cargo"
  export DC_TARGETS_CARGO="$d/bin/cargo" DC_TARGETS_RUSTUP="$d/bin/rustup"
  printf '%s\n' "${TARGETS[@]}" > "$d/installed"
  expect 0 "all three targets compile: one ok line each and the output says compile-only" "ok: aarch64-apple-darwin" "$self" "$d/r"
  out=$("$self" "$d/r" 2>&1); [ "$(grep -c '^ok: ' <<<"$out")" = 3 ] && grep -q 'COMPILE-ONLY' <<<"$out" && n=$((n+1)) || { echo "SELFTEST BAD: expected 3 ok lines and COMPILE-ONLY: $out"; bad=$((bad+1)); }
  echo aarch64-apple-darwin > "$d/failtarget"
  expect 1 "a target that fails to compile exits 1 and prints no ok line for it" "FAIL: aarch64-apple-darwin did not compile" "$self" "$d/r"
  out=$("$self" "$d/r" 2>&1); ! grep -q 'ok: aarch64-apple-darwin' <<<"$out" && n=$((n+1)) || { echo "SELFTEST BAD: failed target printed ok"; bad=$((bad+1)); }
  rm "$d/failtarget"
  printf '%s\n' x86_64-unknown-linux-gnu > "$d/installed"
  expect 0 "missing targets are installed once, then checked" "installing missing rustup target aarch64-unknown-linux-gnu" "$self" "$d/r"
  printf '%s\n' x86_64-unknown-linux-gnu > "$d/installed"; : > "$d/addfails"
  expect 2 "a target that cannot be installed exits 2, never 0" "could not install rustup target" "$self" "$d/r"
  rm "$d/addfails"
  DC_TARGETS_RUSTUP="$d/bin/nonexistent-rustup" expect 2 "no rustup exits 2" "rustup not found" "$self" "$d/r"
  echo "selftest: $n ok, $bad bad"; [ $bad = 0 ]
}

if [ "${1:-}" = --selftest ]; then selftest; else check "${1:-.}"; fi

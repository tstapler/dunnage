#!/usr/bin/env bash
# shellcheck disable=SC2015
# Fails when the Bazel build and the Cargo manifests have drifted apart (ADR-008). Cargo.toml/Cargo.lock stay the
# source of truth; Bazel derives from them, so every check here reads both sides.
#   check-bazel-cargo-sync.sh [ROOT]   run all checks (ROOT defaults to the current directory)
#   check-bazel-cargo-sync.sh --selftest
# Checks (each fails closed: a missing file or tool is a FAIL, never a pass):
#   toolchain  rust-toolchain.toml channel == RUST_VERSION in MODULE.bazel, and .bazelversion exists
#   lock       packages in Cargo.lock == crates in cargo-bazel-lock.json (name + version)
#   members    every workspace member has a BUILD.bazel, and every cargo target kind has a Bazel target:
#              tests/*.rs through the glob macro, src/bin, examples and build.rs by being named in BUILD.bazel
#   features   every [features] name of a member appears in a BUILD.bazel (a new feature must be wired to a flag)
#   lints      [workspace.lints.clippy] == build/BUILD.bazel workspace_lints
#   profile    [profile.dist] lto == the lto of `build:dist` in .bazelrc
#   bazel      (when scripts/bazel.sh runs; DC_SYNC_BAZEL=0 skips) `bazel build --nobuild --lockfile_mode=error //...` finds
#              MODULE.bazel.lock current, and `bazel query` lists a //crates/<c>:<stem>_test for every tests/*.rs
set -euo pipefail

die() { echo "FAIL: $*"; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "required tool '$1' not found on PATH (the check cannot run, so it does not pass)"; }

# All static checks, in Python (tomllib) so the TOML is parsed, not grepped. Prints one FAIL line per problem.
static_checks() {
  python3 -I - "$1" <<'PY'
import json, re, sys, tomllib
from pathlib import Path

root = Path(sys.argv[1])
bad = []

def fail(msg): bad.append(msg)

def read(p):
    try: return (root / p).read_text()
    except OSError: fail(f"missing file {p}"); return ""

def toml(p):
    try: return tomllib.loads(read(p))
    except tomllib.TOMLDecodeError as e: fail(f"{p}: {e}"); return {}

cargo = toml("Cargo.toml")
module = read("MODULE.bazel")
rc = read(".bazelrc")

# toolchain
chan = toml("rust-toolchain.toml").get("toolchain", {}).get("channel", "")
m = re.search(r'^RUST_VERSION *= *"([^"]+)"', module, re.M)
if not m: fail('MODULE.bazel has no RUST_VERSION = "x.y.z"')
elif chan != m.group(1): fail(f"rust-toolchain.toml channel {chan!r} != MODULE.bazel RUST_VERSION {m.group(1)!r}")
if not re.fullmatch(r"\d+\.\d+\.\d+", chan): fail(f"rust-toolchain.toml channel {chan!r} is not an explicit x.y.z version")
if not (root / ".bazelversion").is_file(): fail(".bazelversion missing")

# lock: Cargo.lock packages == crate_universe lock
lock = toml("Cargo.lock").get("package", [])
want = {f'{p["name"]} {p["version"]}' for p in lock}
try: have = set(json.loads(read("cargo-bazel-lock.json")).get("crates", {}))
except json.JSONDecodeError as e: fail(f"cargo-bazel-lock.json: {e}"); have = set()
if not want: fail("Cargo.lock lists no packages")
for k in sorted(want - have): fail(f"Cargo.lock has {k} but cargo-bazel-lock.json does not (repin: CARGO_BAZEL_REPIN=1 scripts/bazel.sh query @crates//:all)")
for k in sorted(have - want): fail(f"cargo-bazel-lock.json has {k} but Cargo.lock does not (repin: CARGO_BAZEL_REPIN=1 scripts/bazel.sh query @crates//:all)")

# members
members = cargo.get("workspace", {}).get("members", [])
if not members: fail("Cargo.toml has no [workspace] members")
all_features = {}
build_text = {}
for d in members:
    b = read(f"{d}/BUILD.bazel")
    build_text[d] = b
    if not b: continue
    man = toml(f"{d}/Cargo.toml")
    all_features[d] = list(man.get("features", {}))
    for t in sorted((root / d / "tests").glob("*.rs")):
        pass  # covered by the glob macro below
    if (root / d / "tests").is_dir() and "dc_integration_tests(" not in b:
        fail(f"{d}/BUILD.bazel has no dc_integration_tests(...) but {d}/tests exists")
    for sub in sorted((root / d / "tests").glob("*/main.rs")):
        fail(f"{sub.relative_to(root)} is a Cargo test target (tests/<dir>/main.rs) that dc_integration_tests does not cover")
    for f in sorted((root / d / "src" / "bin").glob("*.rs")) + sorted((root / d / "examples").glob("*.rs")):
        rel = f.relative_to(root / d).as_posix()
        if rel not in b: fail(f"{d}/{rel} is a Cargo target with no Bazel target (name it in {d}/BUILD.bazel)")
    if (root / d / "build.rs").is_file() and "build.rs" not in b and "DC_GIT_COMMIT" not in b:
        fail(f"{d}/build.rs exists but {d}/BUILD.bazel neither builds nor replaces it")
    for sec in ("bench", "test", "example", "bin"):
        for t in man.get(sec, []):
            p = t.get("path", "")
            if p and p not in b and f"{t.get('name','')}" not in b: fail(f"{d}/Cargo.toml [[{sec}]] {t.get('name')} has no Bazel target")
    if "[lib]" not in read(f"{d}/Cargo.toml") and not (root / d / "src" / "lib.rs").is_file() and not (root / d / "src" / "main.rs").is_file():
        fail(f"{d} has neither src/lib.rs nor src/main.rs")
pk = [p for p in (root / "crates").glob("*/Cargo.toml")] if (root / "crates").is_dir() else []
for p in pk:
    d = p.parent.relative_to(root).as_posix()
    if d not in members: fail(f"{d} has a Cargo.toml but is not in [workspace] members")

# features
joined = "\n".join(build_text.values()) + "\n" + read("build/BUILD.bazel")
for d, feats in all_features.items():
    for f in feats:
        if f == "default": continue
        if f'"{f}"' not in joined: fail(f'{d}: Cargo feature "{f}" is not wired in any BUILD.bazel (a flag + select, or the _ts variant)')

# lints
want_l = {k: (v if isinstance(v, str) else v["level"]) for k, v in cargo.get("workspace", {}).get("lints", {}).get("clippy", {}).items()}
bb = read("build/BUILD.bazel")
mm = re.search(r'rust_lint_config\(\s*name = "workspace_lints",.*?clippy = \{(.*?)\},', bb, re.S)
have_l = dict(re.findall(r'"([a-z_]+)": "([a-z-]+)"', mm.group(1))) if mm else {}
if not mm: fail("build/BUILD.bazel has no rust_lint_config(name = \"workspace_lints\", clippy = {...})")
if not want_l: fail("Cargo.toml has no [workspace.lints.clippy]")
for k in sorted(set(want_l) | set(have_l)):
    if want_l.get(k) != have_l.get(k): fail(f"clippy lint {k}: Cargo.toml {want_l.get(k)!r} vs build/BUILD.bazel {have_l.get(k)!r}")

# profile
lto = cargo.get("profile", {}).get("dist", {}).get("lto")
rl = re.search(r"^build:dist +--@rules_rust//rust/settings:lto=(\S+)", rc, re.M)
if lto is None or not rl or rl.group(1) != lto: fail(f"[profile.dist] lto = {lto!r} vs .bazelrc build:dist lto = {rl.group(1) if rl else None!r}")
if cargo.get("profile", {}).get("dist", {}).get("inherits") != "release" or not re.search(r"^build:dist +--compilation_mode=opt", rc, re.M):
    fail("[profile.dist] must inherit release and .bazelrc build:dist must be --compilation_mode=opt")

for b in bad: print("FAIL:", b)
sys.exit(1 if bad else 0)
PY
}

# Needs Bazel: MODULE.bazel.lock current, and a target for every tests/*.rs.
bazel_checks() {
  local root=$1 bz=${DC_BAZEL:-$1/scripts/bazel.sh} rc=0 targets crate f stem
  [ -x "$bz" ] || command -v "$bz" >/dev/null 2>&1 || { echo "FAIL: $bz not found (DC_SYNC_BAZEL=0 skips the Bazel checks)"; return 1; }
  (cd "$root" && "$bz" build --nobuild --lockfile_mode=error //... >/dev/null 2>&1) || { echo "FAIL: MODULE.bazel.lock is out of date (run: scripts/bazel.sh build --nobuild --lockfile_mode=update //...) or bazel failed"; rc=1; }
  # A Cargo.toml dependency change (a new dev-dependency, a bumped version) leaves cargo-bazel-lock.json stale, and
  # Bazel then fails later with "cannot find crate": re-pin in place and fail if the committed lock changes.
  if [ "${DC_SYNC_REPIN:-1}" != 0 ]; then
    local before after
    before=$(sha256sum "$root/cargo-bazel-lock.json" 2>/dev/null | cut -d' ' -f1)
    (cd "$root" && CARGO_BAZEL_REPIN=1 "$bz" build --nobuild //crates/cli:cli_ts >/dev/null 2>&1)
    after=$(sha256sum "$root/cargo-bazel-lock.json" 2>/dev/null | cut -d' ' -f1)
    if [ "$before" != "$after" ]; then
      echo "FAIL: cargo-bazel-lock.json was stale: a Cargo.toml dependency changed (it is now re-pinned in the working tree: review and commit it)"; rc=1
    fi
  fi
  targets=$(cd "$root" && "$bz" query 'kind("rust_test", //crates/...)' 2>/dev/null) || { echo "FAIL: bazel query failed"; return 1; }
  for f in "$root"/crates/*/tests/*.rs; do
    [ -e "$f" ] || continue
    crate=${f#"$root"/crates/}; crate=${crate%%/*}; stem=${f##*/}; stem=${stem%.rs}
    printf '%s\n' "$targets" | grep -qx "//crates/$crate:${stem}_test" || { echo "FAIL: crates/$crate/tests/$stem.rs has no Bazel target //crates/$crate:${stem}_test"; rc=1; }
  done
  return "$rc"
}

run() {
  local root=${1:-.} rc=0
  need python3
  root=$(cd "$root" && pwd)
  static_checks "$root" || rc=1
  if [ "${DC_SYNC_BAZEL:-1}" != 0 ]; then bazel_checks "$root" || rc=1; fi
  [ "$rc" -eq 0 ] && echo "ok: Cargo and Bazel agree (toolchain, lock, members, features, lints, profile$([ "${DC_SYNC_BAZEL:-1}" != 0 ] && echo ', bazel'))"
  return "$rc"
}

# A minimal synthetic repo that passes, then one mutation per check that must fail.
make_good() {
  local d=$1
  mkdir -p "$d/crates/a/tests/sub" "$d/crates/a/src/bin" "$d/build"
  cat > "$d/Cargo.toml" <<'EOF'
[workspace]
members = ["crates/a"]
[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
unwrap_used = "warn"
cast_sign_loss = "allow"
[profile.dist]
inherits = "release"
lto = "thin"
EOF
  printf '[package]\nname = "a"\nversion = "0.1.0"\n[features]\ndefault = []\ngate-b = []\n' > "$d/crates/a/Cargo.toml"
  printf 'RUST_VERSION = "1.98.1"\n' > "$d/MODULE.bazel"
  printf '[toolchain]\nchannel = "1.98.1"\n' > "$d/rust-toolchain.toml"
  echo 9.3.2 > "$d/.bazelversion"
  printf 'build:dist --compilation_mode=opt\nbuild:dist --@rules_rust//rust/settings:lto=thin\n' > "$d/.bazelrc"
  printf '[[package]]\nname = "a"\nversion = "0.1.0"\n[[package]]\nname = "serde"\nversion = "1.0.0"\nsource = "registry+x"\n' > "$d/Cargo.lock"
  echo '{"crates":{"a 0.1.0":{},"serde 1.0.0":{}}}' > "$d/cargo-bazel-lock.json"
  printf 'rust_lint_config(\n    name = "workspace_lints",\n    clippy = {\n        "all": "warn",\n        "unwrap_used": "warn",\n        "cast_sign_loss": "allow",\n    },\n)\n' > "$d/build/BUILD.bazel"
  printf 'dc_integration_tests(crate = "a")\nFEATURES = ["gate-b"]\nsrcs = ["src/bin/tool.rs"]\n' > "$d/crates/a/BUILD.bazel"
  echo 'fn main() {}' > "$d/crates/a/src/bin/tool.rs"; echo 'fn main() {}' > "$d/crates/a/src/lib.rs"; echo '' > "$d/crates/a/tests/one.rs"
}

selftest() {
  need python3
  local n=0 bad=0 d out rc
  t=$(mktemp -d); trap 'rm -rf "$t"' EXIT INT TERM   # global: the EXIT trap runs after this function's locals are gone
  export DC_SYNC_BAZEL=0
  expect() {  # want(pass|fail) name pattern mutation-command...
    local want=$1 name=$2 pat=$3; shift 3
    d=$t/r$n; make_good "$d"; (cd "$d" && eval "$*")
    rc=0; out=$(run "$d" 2>&1) || rc=$?
    if { [ "$want" = pass ] && [ $rc -eq 0 ]; } || { [ "$want" = fail ] && [ $rc -eq 1 ]; }; then
      if echo "$out" | grep -q -- "$pat"; then echo "ok: $name"; else echo "SELFTEST BAD: $name (output lacks '$pat'): $out"; bad=$((bad+1)); fi
    else echo "SELFTEST BAD: $name (rc=$rc): $out"; bad=$((bad+1)); fi
    n=$((n+1))
  }
  expect pass "a consistent repo passes" "ok: Cargo and Bazel agree" true
  expect fail "toolchain version drift" "rust-toolchain.toml channel" "sed -i 's/1.98.1/1.99.0/' MODULE.bazel"
  expect fail "a floating toolchain channel" "not an explicit x.y.z" "printf '[toolchain]\nchannel = \"stable\"\n' > rust-toolchain.toml; sed -i 's/1.98.1/stable/' MODULE.bazel"
  expect fail "a missing .bazelversion" ".bazelversion missing" "rm .bazelversion"
  expect fail "a package missing from the crate lock" "Cargo.lock has serde 1.0.0" "echo '{\"crates\":{\"a 0.1.0\":{}}}' > cargo-bazel-lock.json"
  expect fail "a stale package in the crate lock" "cargo-bazel-lock.json has old 0.9.0" "echo '{\"crates\":{\"a 0.1.0\":{},\"serde 1.0.0\":{},\"old 0.9.0\":{}}}' > cargo-bazel-lock.json"
  expect fail "a version bump in Cargo.lock alone" "Cargo.lock has serde 1.0.1" "sed -i 's/1.0.0/1.0.1/' Cargo.lock"
  expect fail "a member without BUILD.bazel" "missing file crates/a/BUILD.bazel" "rm crates/a/BUILD.bazel"
  expect fail "tests/ without the glob macro" "no dc_integration_tests" "echo 'FEATURES = [\"gate-b\"] # src/bin/tool.rs' > crates/a/BUILD.bazel"
  expect fail "a src/bin target nobody built" "src/bin/extra.rs is a Cargo target with no Bazel target" "echo 'fn main() {}' > crates/a/src/bin/extra.rs"
  expect fail "an example nobody built" "examples/demo.rs is a Cargo target" "mkdir crates/a/examples; echo 'fn main() {}' > crates/a/examples/demo.rs"
  expect fail "a tests/<dir>/main.rs the macro would miss" "tests/sub/main.rs is a Cargo test target" "echo '' > crates/a/tests/sub/main.rs"
  expect fail "a crate not in the workspace" "crates/b has a Cargo.toml but is not in" "mkdir crates/b; echo '[package]' > crates/b/Cargo.toml"
  expect fail "a feature with no flag" 'Cargo feature "gate-c" is not wired' "sed -i 's/gate-b = \\[\\]/gate-b = []\\ngate-c = []/' crates/a/Cargo.toml"
  expect fail "a lint level changed in Cargo.toml" "clippy lint unwrap_used" "sed -i 's/unwrap_used = \"warn\"/unwrap_used = \"deny\"/' Cargo.toml"
  expect fail "a lint added in Cargo.toml only" "clippy lint pedantic" "sed -i 's/^unwrap_used/pedantic = \"warn\"\\nunwrap_used/' Cargo.toml"
  expect fail "a lint dropped from Bazel" "clippy lint cast_sign_loss" "sed -i '/cast_sign_loss/d' build/BUILD.bazel"
  expect fail "dist lto drift" "lto = 'thin' vs .bazelrc build:dist lto = None" "sed -i '/lto=thin/d' .bazelrc"
  expect fail "a missing Cargo.lock" "missing file Cargo.lock" "rm Cargo.lock"
  echo "selftest: $((n - bad)) ok, $bad bad"; [ "$bad" -eq 0 ]
}

case "${1:-}" in
  --selftest) selftest ;;
  -h | --help) sed -n '2,18p' "$0" ;;
  *) run "${1:-.}" ;;
esac

#!/usr/bin/env bash
# shellcheck disable=SC2154,SC2015
# Who may mint an ApplyToken, an Allowed or an unconfined RealDirOps. AST-aware (ast-grep), so comments,
# strings, macros, aliases, fn pointers and line breaks cannot hide or fake a site.
#   check-token-sites.sh --expect-sites N [repo-root]   N = number of call sites that must exist for
#                                                       grant and for unconfined (0 before Task 2.1.6a, 1 after)
#   check-token-sites.sh --selftest                     positive controls + one negative per evasion
# Scans every *.rs under the root (all workspace members, xtask/, build.rs), not only crates/.
# Raw identifiers (r#grant) are normalised before matching. Binary choice: `ast-grep` first; `sg` only if it
# identifies itself as ast-grep (on Debian/Ubuntu /usr/bin/sg is the shadow-utils group command).
set -uo pipefail

DEF_TOKEN=crates/core/src/apply_token.rs     # defines ApplyToken, grant, for_tests
DEF_DIROPS=crates/adapters/src/dirops.rs     # defines RealDirOps::unconfined
SITE_GRANT=crates/cli/src/wiring/token.rs    # the one place that may call grant
SITE_UNCONF=crates/cli/src/wiring/confine.rs # the one place that may call unconfined
GUARD=crates/core/src/guard.rs               # the one place that may call Allowed::new
WITNESS_TYPES='ApplyToken|Allowed|WorktreeRemoval'
# the only adapters files that may contain `unsafe` (fiemap.rs also answers to the content rule below) (a ZST such as ApplyToken can be forged by any unsafe pointer cast)
UNSAFE_OK='^crates/adapters/src/(dirops|space_meter|lock|signals|fiemap)\.rs:|^crates/adapters/src/probe/|^crates/scan/src/btrfs_tree\.rs:'
# the scan crate is a leaf that cannot name a witness type (check-scan-deps.sh); its one ioctl module builds the
# BTRFS_IOC_TREE_SEARCH_V2 request header with slice::from_raw_parts, and only that identifier in that file is exempt
FORGE_OK='^crates/scan/src/btrfs_tree\.rs:[0-9]+:from_raw_parts$'

SELF=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")

FIEMAP=crates/adapters/src/fiemap.rs         # the one `unsafe` file for shared-extent attribution (ADR-004); read-only by content

# The content rule for $FIEMAP: an ALLOW list of names, so an unlisted sibling spelling fails by not being listed
# (reference and evasion list: tui/tools/fiemap_scan.py of the plan). Written to a temp file and run with `python3 -I`.
write_fiemap_scan() {  # $1 = path
  cat > "$1" <<'PYEOF'
"""Reference for the content rule that `scripts/check-token-sites.sh` applies to the one `unsafe` file,
`adapters/src/fiemap.rs` (plan Task 1.2.2d, ADR-004 outcome 3; Story 6.3.2 extends the allow table). An ALLOW list, not a deny list: the file may name
only the external paths, imported members, macros and attributes below, so an unlisted sibling spelling (`renameat`, `remove_file`,
`fchmod`, `OFlags::WRONLY`, `libc::syscall`, a macro-built call) fails by not being listed. A second layer rejects identifiers that
contain a write-like word. The text is tokenised (strings and comments removed by a small Rust lexer, so `"//"` cannot hide code).
Usage: python3 -I fiemap_scan.py <file.rs>            prints "file:line: rule: text" per hit, exit 1 on any hit
       python3 -I fiemap_scan.py --selftest <scratch>  plants the evasions and the clean files, exit 0 when all behave"""
import os
import re
import sys

ALLOWED_PATHS = {  # every external path the file may spell, as a `use` leaf or fully qualified
    "rustix::fs::open", "rustix::fs::lstat", "rustix::fs::fstat", "rustix::fs::statfs",
    "rustix::fs::OFlags", "rustix::fs::Mode", "rustix::fs::FileType",
    "libc::ioctl", "libc::c_int", "libc::c_ulong", "libc::c_void",
    "core::mem::size_of", "core::mem::zeroed",
    "std::io", "std::io::Error", "std::os::fd::AsRawFd", "std::os::fd::OwnedFd", "std::path::Path",
    "std::time::Duration", "std::time::Instant", "std::sync::atomic::AtomicBool", "std::sync::atomic::Ordering",
    "disk_cleanup_core::ports::ExtentProbe", "disk_cleanup_core::ports::ExtentAttribution",
    "disk_cleanup_core::ports::Unavailable", "disk_cleanup_core::ports::ItemExtents",
    "disk_cleanup_core::ports::SharedExtent", "disk_cleanup_core::explore::SharedEstimate",
}
ALLOWED_MEMBERS = {  # Type::member of a name imported from rustix or libc
    "OFlags::RDONLY", "OFlags::NOFOLLOW", "OFlags::NONBLOCK", "OFlags::CLOEXEC", "Mode::empty",
    "FileType::RegularFile", "FileType::Symlink", "FileType::Directory", "FileType::from_raw_mode",
}
ALLOWED_MACROS = {"matches", "debug_assert", "debug_assert_eq", "assert", "assert_eq", "cfg", "format", "vec"}
ALLOWED_ATTRS = {"repr", "derive", "cfg", "allow", "inline", "must_use", "test"}
EXTERNAL_ROOTS = {"rustix", "libc", "std", "core", "alloc", "nix", "crate", "super", "disk_cleanup_core",
                  "disk_cleanup_adapters", "metascope_core"}
CAPABILITY_ROOTS = {"rustix", "libc"}
SECOND_LAYER = ("unlink", "rename", "remove", "write", "trunc", "fallocate", "chmod", "chown", "syscall", "wronly", "rdwr",
                "creat", "append", "applytoken", "allowedtarget", "delete", "dirops", "mkdir", "symlink", "linkat",
                "utime", "xattr", "mount", "flag_sync", "openoptions", "transmute")
PUNCT2 = ("::", "->", "=>", "!=", "==", "<=", ">=", "&&", "||", "..")


class Bad(Exception):
    pass


def lex(src):
    """-> list of (kind, text, line); kinds: id, num, p, str, chr, life. Raises Bad on an unterminated string or comment."""
    i, n, line, out = 0, len(src), 1, []
    while i < n:
        c = src[i]
        if c == "\n":
            line += 1; i += 1; continue
        if c in " \t\r":
            i += 1; continue
        if src.startswith("//", i):
            while i < n and src[i] != "\n":
                i += 1
            continue
        if src.startswith("/*", i):
            depth, i = 1, i + 2
            while depth:
                if i >= n:
                    raise Bad(f"{line}: unterminated block comment")
                if src.startswith("/*", i): depth += 1; i += 2
                elif src.startswith("*/", i): depth -= 1; i += 2
                else:
                    line += src[i] == "\n"; i += 1
            continue
        m = re.match(r'(?:br|cr|r|b|c)?(#*)"', src[i:]) if c in "rbc\"" else None
        if m and (c == '"' or src[i:i + 1] in "rbc"):
            hashes, raw = m.group(1), "r" in src[i:i + m.end()]
            i += m.end()
            if raw or hashes:
                end = '"' + hashes
                j = src.find(end, i)
                if j < 0: raise Bad(f"{line}: unterminated raw string")
                line += src[i:j].count("\n"); i = j + len(end)
            else:
                while True:
                    if i >= n: raise Bad(f"{line}: unterminated string")
                    if src[i] == "\\": i += 2; continue
                    if src[i] == '"': i += 1; break
                    line += src[i] == "\n"; i += 1
            out.append(("str", "", line)); continue
        if c == "'":
            m = re.match(r"'(?:\\.[^']*|[^\\'])'", src[i:])
            if m: i += m.end(); out.append(("chr", "", line)); continue
            m = re.match(r"'[A-Za-z_][A-Za-z0-9_]*", src[i:])
            if m: i += m.end(); out.append(("life", m.group(0), line)); continue
            raise Bad(f"{line}: stray quote")
        m = re.match(r"(?:r#)?[A-Za-z_][A-Za-z0-9_]*", src[i:])
        if m: i += m.end(); out.append(("id", m.group(0), line)); continue
        m = re.match(r"0[xob][0-9a-fA-F_]+[A-Za-z0-9_]*|[0-9][0-9_]*(?:\.[0-9_]+)?(?:[eE][+-]?[0-9_]+)?[A-Za-z0-9_]*", src[i:])
        if m: i += m.end(); out.append(("num", m.group(0), line)); continue
        if ord(c) > 127: raise Bad(f"{line}: non-ASCII character outside a string or comment")
        two = src[i:i + 2]
        if two in PUNCT2: out.append(("p", two, line)); i += 2; continue
        out.append(("p", c, line)); i += 1
    return out


def use_leaves(t, i):
    """Expand the use tree starting after `use` at index i; returns (leaves, next index, problems)."""
    leaves, probs = [], []

    def tree(i, prefix):
        path = list(prefix)
        while i < len(t):
            k, x, ln = t[i]
            if k == "id" and x not in ("as", "self"):
                path.append(x); i += 1
                if t[i][1] == "::": i += 1; continue
                break
            if x == "self": i += 1; break
            if x == "*": probs.append((ln, "glob import")); i += 1; return i
            if x == "{":
                i += 1
                while t[i][1] != "}":
                    i = tree(i, path)
                    if t[i][1] == ",": i += 1
                return i + 1
            if x == "::": i += 1; continue
            break
        if i < len(t) and t[i][1] == "as":
            probs.append((t[i][2], "use ... as alias")); i += 2
        leaves.append(("::".join(path), t[min(i, len(t) - 1)][2]))
        return i

    i = tree(i, [])
    return leaves, i, probs


def second_arg(t, i):
    """Token texts of the second top-level argument of the call whose '(' is t[i]."""
    depth, args, cur = 0, [], []
    for _, x, _ in t[i:]:
        depth += x in ("(", "[", "{")
        depth -= x in (")", "]", "}")
        if depth == 0:
            break
        if depth == 1 and x == ",":
            args.append(cur); cur = []
        elif not (depth == 1 and x == "("):
            cur.append(x)
    args.append(cur)
    return args[1] if len(args) > 1 else []


def scan(src, name="fiemap.rs"):
    hits = []

    def hit(ln, rule, text): hits.append(f"{name}:{ln}: {rule}: {text}")

    try:
        t = lex(src)
    except Bad as e:
        return [f"{name}: lex: {e}"]
    imported = {}  # local name -> full path
    i = 0
    while i < len(t):
        k, x, ln = t[i]
        if k == "id" and x == "use":
            leaves, j, probs = use_leaves(t, i + 1)
            for pl, pt in probs: hit(pl, "use", pt)
            for path, pln in leaves:
                if path not in ALLOWED_PATHS and not path.startswith(("super", "self", "crate")) and path.split("::")[0] in EXTERNAL_ROOTS:
                    hit(pln, "path", f"{path} is not on the allow list")
                if path.split("::")[0] in ("crate", "super"): hit(pln, "path", f"{path}: crate/super paths are not allowed")
                imported[path.split("::")[-1]] = path.split("::")[0]
            while j < len(t) and t[j][1] != ";": j += 1
            i = j + 1; continue
        i += 1
    ioctl_calls, defs = 0, 0
    i = 0
    while i < len(t):
        k, x, ln = t[i]
        if k == "id":
            if x == "use":
                while t[i][1] != ";": i += 1
                i += 1; continue
            low = x.lower()
            for w in SECOND_LAYER:
                if w in low: hit(ln, "second-layer", f"identifier {x} contains {w}")
            if x in ("extern", "macro_rules", "asm", "global_asm", "include", "include_str", "include_bytes", "paste"):
                hit(ln, "construct", x)
            prev = t[i - 1][1] if i else ""
            if prev != "::" and prev != ".":  # start of a path
                segs, j = [x], i + 1
                while j + 1 < len(t) and t[j][1] == "::":
                    if t[j + 1][1] == "<":  # turbofish: skip balanced <...>
                        d, j = 0, j + 1
                        while True:
                            d += t[j][1] == "<"; d -= t[j][1] == ">"; j += 1
                            if d == 0: break
                        continue
                    segs.append(t[j + 1][1]); j += 2
                if segs[0] in EXTERNAL_ROOTS:
                    full = "::".join(segs)
                    if segs[0] in ("crate", "super") or full not in ALLOWED_PATHS:
                        hit(ln, "path", f"{'::'.join(segs)} is not on the allow list")
                elif imported.get(segs[0]) in CAPABILITY_ROOTS and len(segs) >= 2 and "::".join(segs[:2]) not in ALLOWED_MEMBERS:
                    hit(ln, "member", f"{'::'.join(segs[:2])} is not on the allow list")
            if x == "ioctl" and i + 1 < len(t) and t[i + 1][1] == "(":
                ioctl_calls += 1
                ok = second_arg(t, i + 1) == ["FS_IOC_FIEMAP"]
                if not ok: hit(ln, "ioctl", "the one ioctl call must be ioctl(fd, FS_IOC_FIEMAP, ...)")
            if x == "FS_IOC_FIEMAP" and t[i - 1][1] == "const":
                defs += 1
                if not (t[i + 1][1] == ":" and any(tok[1] == "0xC020660B" or tok[1].lower().replace("_", "") == "0xc020660b" for tok in t[i + 2:i + 12])):
                    hit(ln, "ioctl", "FS_IOC_FIEMAP must be defined as 0xC020660B")
            if x == "fm_flags":
                nxt = t[i + 1][1] if i + 1 < len(t) else ""
                val = t[i + 2][1] if i + 2 < len(t) else ""
                if nxt in (":", "=") and val not in ("0", "u32", "u64", "c_uint"): hit(ln, "flags", "fm_flags may only be set to 0")
            if i + 2 < len(t) and t[i + 1][1] == "!" and t[i + 2][1] in ("(", "[", "{") and x not in ALLOWED_MACROS and x not in ("macro_rules",):
                hit(ln, "macro", f"{x}! is not on the allow list")
        elif x == "$":
            hit(ln, "construct", "$ (macro metavariable)")
        elif x == "#" and i + 1 < len(t):
            j = i + 1 + (t[i + 1][1] == "!")
            if t[j][1] == "[" and t[j + 1][1] not in ALLOWED_ATTRS and t[j + 1][1] not in ("forbid", "deny"):
                hit(ln, "attr", f"#[{t[j + 1][1]}] is not on the allow list")
        i += 1
    if ioctl_calls > 1: hit(0, "ioctl", f"{ioctl_calls} ioctl calls (exactly one allowed)")
    if defs > 1: hit(0, "ioctl", "FS_IOC_FIEMAP defined more than once")
    return hits


CLEAN_FULL = '''//! the probe; "//" inside a string is not a comment
use rustix::fs::{lstat, open, statfs, FileType, Mode, OFlags};
use libc::{c_int, c_ulong};
use core::mem::size_of;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};
use disk_cleanup_core::ports::{ExtentAttribution, ExtentProbe, Unavailable};
const FS_IOC_FIEMAP: c_ulong = 0xC020_660B;
const FIEMAP_EXTENT_SHARED: u32 = 0x0000_2000;
#[repr(C)] struct Fiemap { fm_start: u64, fm_length: u64, fm_flags: u32, fm_extent_count: u32 }
fn probe(p: &std::path::Path, budget: Duration) -> bool {
    let t0 = Instant::now();
    let st = lstat(p).ok();
    let fd = open(p, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK, Mode::empty()).ok();
    let mut fm = Fiemap { fm_start: 0, fm_length: u64::MAX, fm_flags: 0, fm_extent_count: 0 };
    let _ = size_of::<Fiemap>();
    let _ = matches!(st.map(|s| FileType::from_raw_mode(s.st_mode)), Some(FileType::RegularFile));
    let r = unsafe { libc::ioctl(fd.unwrap().as_raw_fd() as c_int, FS_IOC_FIEMAP, &mut fm as *mut Fiemap) };
    r == 0 && t0.elapsed() < budget && fm.fm_extent_count & FIEMAP_EXTENT_SHARED == 0
}
'''
CLEAN_MIN = 'use disk_cleanup_core::ports::ExtentProbe;\nfn f() { let _ = "std::fs::remove_file"; /* rustix::fs::unlinkat */ }\n'
EVASIONS = {
    "01_renameat": 'fn f(){ rustix::fs::renameat(a, b, c, d); }',
    "02_remove_file": 'use std::fs::remove_file; fn f(){ remove_file("a"); }',
    "03_fchmod": 'use rustix::fs::fchmod; fn f(){ }',
    "04_oflags_wronly": 'use rustix::fs::{open, Mode, OFlags}; fn f(){ open(p, OFlags::WRONLY, Mode::empty()); }',
    "05_libc_syscall": 'fn f(){ unsafe { libc::syscall(87, p) }; }',
    "06_macro_built_call": 'macro_rules! m { ($f:ident) => { rustix::fs::$f(p) } } fn f(){ m!(unlink); }',
    "07_alias": 'use rustix::fs::open as o; fn f(){ }',
    "08_glob": 'use rustix::fs::*; fn f(){ }',
    "09_extern_block": 'extern "C" { fn unlink(p: *const i8) -> i32; } fn f(){}',
    "10_libc_write": 'fn f(){ unsafe { libc::write(1, p, 1) }; }',
    "11_second_ioctl": 'const FS_IOC_FIEMAP: c_ulong = 0xC020660B; fn f(){ unsafe { libc::ioctl(fd, 0x40086602, p) }; }',
    "12_sync_flag": 'fn f(){ let fm = Fiemap { fm_flags: FIEMAP_FLAG_SYNC }; }',
    "13_asm": 'fn f(){ unsafe { core::arch::asm!("syscall"); } }',
    "14_include": 'include!("../delete.rs");',
    "15_string_hides_code": 'fn f(){ let s = "//"; std::fs::remove_file(p); }',
    "16_path_attr": '#[path = "../delete.rs"] mod d;',
    "17_apply_token": 'fn f(t: ApplyToken){}',
    "18_unlisted_macro": 'fn f(){ mymac!(p); }',
    "19_raw_ident": 'fn f(){ r#unlinkat(p); }',
    "20_transmute": 'fn f(){ unsafe { core::mem::transmute::<u8, u8>(1) }; }',
    "21_process": 'use std::process::Command; fn f(){ Command::new("rm"); }',
    "22_crate_path": 'fn f(){ crate::delete::tree(p); }',
    "23_oflags_create": 'use rustix::fs::OFlags; fn f(){ let _ = OFlags::CREATE; }',
    "24_flags_nonzero": 'fn f(){ let fm = Fiemap { fm_flags: 1 }; }',
    "25_write_all_method": 'fn f(){ file.write_all(b"x"); }',
}


def selftest(root):
    d = os.path.join(root, "fiemap-selftest"); fail = 0
    os.makedirs(d, exist_ok=True)
    for name, text in EVASIONS.items():
        if not scan(text, name):
            print("MISSED:", name); fail = 1
    for name, text in (("clean_full", CLEAN_FULL), ("clean_min", CLEAN_MIN)):
        h = scan(text, name)
        if h: print("FALSE POSITIVE:", *h, sep="\n"); fail = 1
    print(f"controls: {len(EVASIONS)} evasions planted, clean files: 2, failures: {fail}")
    return fail


if __name__ == "__main__":
    if sys.argv[1] == "--selftest":
        sys.exit(selftest(sys.argv[2]))
    out = scan(open(sys.argv[1]).read(), os.path.basename(sys.argv[1]))
    print("\n".join(out) if out else "ok")
    sys.exit(1 if out else 0)
PYEOF
}

find_sg() {  # prints the ast-grep binary, or nothing
  local b
  for b in ast-grep sg; do
    command -v "$b" >/dev/null 2>&1 && "$b" --version 2>/dev/null | grep -qi 'ast-grep' && { echo "$b"; return 0; }
  done
  return 1
}
need() { command -v "$1" >/dev/null 2>&1 || { echo "FAIL: required tool '$1' not found on PATH (the check cannot run, so it does not pass)"; exit 2; }; }

mkrules() {  # $1 = dir to write rule files into
  local d="$1"
  cat > "$d/ident.yml" <<'EOF'
id: ident
language: rust
rule:
  any: [ { kind: identifier }, { kind: field_identifier } ]
  regex: '^(r#)?(grant|unconfined|for_tests|transmute|transmute_copy|zeroed|MaybeUninit|assume_init|assume_init_read|uninit|uninitialized|dangling|dangling_mut|NonNull|ManuallyDrop|read_unaligned|read_volatile|from_raw|from_raw_parts)$'
EOF
  cat > "$d/ptrread.yml" <<'EOF'
id: ptrread
language: rust
rule: { kind: scoped_identifier, regex: '(^|::)(r#)?ptr::(r#)?(read|read_unaligned|read_volatile)$' }
EOF
  cat > "$d/useptr.yml" <<'EOF'
id: useptr
language: rust
rule: { kind: use_declaration, regex: 'ptr::(\*|\{|read)' }
EOF
  cat > "$d/union.yml" <<'EOF'
id: union
language: rust
rule: { kind: union_item }
EOF
  cat > "$d/ptrtype.yml" <<'EOF'
id: ptrtype
language: rust
rule:
  kind: pointer_type
  has: { any: [ { kind: type_identifier }, { kind: identifier } ], regex: '^(r#)?(ApplyToken|Allowed|WorktreeRemoval)$', stopBy: end }
EOF
  cat > "$d/unsafewitness.yml" <<'EOF'
id: unsafewitness
language: rust
rule:
  any: [ { kind: unsafe_block }, { kind: function_item, has: { kind: function_modifiers, regex: unsafe } } ]
  has: { any: [ { kind: type_identifier }, { kind: identifier } ], regex: '^(r#)?(ApplyToken|Allowed|WorktreeRemoval)$', stopBy: end }
EOF
  cat > "$d/allowednew.yml" <<'EOF'
id: allowednew
language: rust
rule:
  any:
    - { kind: scoped_identifier, regex: '(^|::)(r#)?Allowed::(r#)?new$' }
    - { kind: scoped_identifier, regex: '^<\s*(r#)?Allowed\s*>::(r#)?new$' }
    - { kind: token_tree, regex: '(r#)?Allowed\s*::\s*(r#)?new' }
EOF
  cat > "$d/unsafe.yml" <<'EOF'
id: unsafe
language: rust
rule:
  any:
    - { kind: unsafe_block }
    - { kind: function_modifiers, regex: unsafe }
    - { kind: impl_item, regex: '^unsafe' }
    - { kind: trait_item, regex: '^unsafe' }
    - { kind: token_tree, regex: '\bunsafe\b' }
EOF
  cat > "$d/forbid.yml" <<'EOF'
id: forbid
language: rust
rule: { kind: inner_attribute_item, regex: 'forbid\s*\(\s*unsafe_code\s*\)' }
EOF
  cat > "$d/macro.yml" <<'EOF'
id: macro
language: rust
rule:
  kind: macro_definition
  has: { kind: identifier, regex: '^(r#)?(ApplyToken|Allowed|WorktreeRemoval)$', stopBy: end }
EOF
  cat > "$d/macroinv.yml" <<'EOF'
id: macroinv
language: rust
rule:
  kind: macro_invocation
  has: { field: macro, regex: '^(include|paste|concat_idents)$' }
EOF
  cat > "$d/useas.yml" <<'EOF'
id: useas
language: rust
rule: { kind: use_as_clause, regex: '\b(ApplyToken|Allowed|WorktreeRemoval)\b' }
EOF
  cat > "$d/typealias.yml" <<'EOF'
id: typealias
language: rust
rule: { kind: type_item, regex: '\b(ApplyToken|Allowed|WorktreeRemoval)\b' }
EOF
  cat > "$d/literal.yml" <<'EOF'
id: literal
language: rust
rule:
  all:
    - any:
        - { pattern: 'ApplyToken($$$A)' }
        - { pattern: 'ApplyToken { $$$A }' }
        - { pattern: 'r#ApplyToken($$$A)' }
        - { pattern: 'Self($$$A)' }
        - { pattern: 'Self { $$$A }' }
    - not:
        inside:
          kind: function_item
          has: { field: name, regex: '^(grant|for_tests)$' }
          stopBy: { kind: function_item }
EOF
  cat > "$d/ctorfn.yml" <<'EOF'
id: ctorfn
language: rust
rule:
  kind: function_item
  inside: { kind: impl_item, has: { field: type, regex: '(^|::)(r#)?ApplyToken$' }, stopBy: end }
  not: { has: { field: name, regex: '^(grant|for_tests)$' } }
EOF
  cat > "$d/traitimpl.yml" <<EOF
id: traitimpl
language: rust
rule:
  all:
    - { kind: impl_item }
    - { has: { field: trait, regex: '.' } }
    - { has: { field: type, regex: '\\b(r#)?($WITNESS_TYPES)\\b' } }
EOF
  cat > "$d/derive-token.yml" <<'EOF'
id: derive-token
language: rust
rule:
  kind: attribute_item
  regex: 'derive'
  precedes: { kind: struct_item, has: { field: name, regex: '^ApplyToken$' }, stopBy: { kind: struct_item } }
EOF
  cat > "$d/derive-witness.yml" <<'EOF'
id: derive-witness
language: rust
rule:
  kind: attribute_item
  regex: 'derive\s*\([^)]*\b(Default|Clone|Copy|Deserialize|Serialize|PartialEq|Eq|Hash|PartialOrd|Ord)\b'
  precedes: { kind: struct_item, has: { field: name, regex: '^(Allowed|WorktreeRemoval)$' }, stopBy: { kind: struct_item } }
EOF
}

main() {
  local expect="" root="."
  while [ $# -gt 0 ]; do
    case "$1" in
      --expect-sites) expect="${2:-}"; shift 2 ;;
      --selftest) selftest; return $? ;;
      -*) echo "usage: $0 --expect-sites N [repo-root] | --selftest" >&2; return 64 ;;
      *) root="$1"; shift ;;
    esac
  done
  case "$expect" in 0|1) ;; *) echo "usage: $0 --expect-sites 0|1 [repo-root] (the flag is mandatory so the check cannot be loosened by omission)" >&2; return 64 ;; esac
  need jq
  local SG; SG=$(find_sg) || { echo "FAIL: required tool 'ast-grep' not found on PATH (an 'sg' that is not ast-grep does not count) (the check cannot run, so it does not pass)"; return 2; }
  cd "$root" || return 2
  tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
  mkrules "$tmp"
  find . -name '*.rs' -not -path '*/target/*' -not -path './.git/*' -print0 | sed -z 's|^\./||' > "$tmp/files0"
  [ -s "$tmp/files0" ] || { echo "FAIL: no .rs files under $root (a check that scans nothing passes nothing)"; return 1; }

  hits() {  # $1 = rule name -> "path:line:text" per hit
    xargs -0 "$SG" scan -r "$tmp/$1.yml" --json=compact < "$tmp/files0" 2>"$tmp/err" |
      jq -r '.[] | "\(.file):\(.range.start.line + 1):\(.text | sub("^r#";"") | split("\n")[0] | .[0:60])"'
    [ -s "$tmp/err" ] && { cat "$tmp/err" >&2; echo "FAIL: ast-grep reported an error for rule $1" >&2; return 2; }
    return 0
  }
  local fail=0
  bad() { echo "FAIL: $*"; fail=1; }
  flag() { local m="$1" l; while IFS= read -r l; do [ -n "$l" ] && bad "$m: $l"; done; }
  count() { printf '%s' "$1" | grep -c . || true; }
  in_file() { printf '%s\n' "$1" | grep -E -- "^$2:" || true; }
  not_in() { printf '%s\n' "$1" | grep -Ev -- "$2" | grep . || true; }

  for r in ident ptrread useptr union ptrtype unsafewitness allowednew unsafe forbid macro macroinv useas typealias literal ctorfn traitimpl derive-token derive-witness; do
    eval "H_${r//-/_}=\$(hits $r)" || { bad "rule $r did not run"; return 1; }
  done

  # grant / unconfined: one definition, exactly $expect call sites, nowhere else (by identifier, so aliases,
  # macro arguments, fn pointers, split lines and Self:: wrappers all count)
  local g u t
  g=$(printf '%s\n' "$H_ident" | awk -F: '$3 == "grant"' || true)
  u=$(printf '%s\n' "$H_ident" | awk -F: '$3 == "unconfined"' || true)
  t=$(printf '%s\n' "$H_ident" | awk -F: '$3 == "for_tests"' || true)
  [ "$(count "$(in_file "$g" "$DEF_TOKEN")")" -eq 1 ] || bad "$DEF_TOKEN must contain exactly one 'grant' identifier (its definition; a Self::grant() wrapper or second use is a second site), found $(count "$(in_file "$g" "$DEF_TOKEN")") (0 also means the scan is blind)"
  [ "$(count "$(in_file "$g" "$SITE_GRANT")")" -eq "$expect" ] || bad "$SITE_GRANT must contain exactly $expect 'grant' identifier(s) (the call site), found $(count "$(in_file "$g" "$SITE_GRANT")")"
  flag "'grant' outside its definition and call site" < <(not_in "$g" "^($DEF_TOKEN|$SITE_GRANT):")
  [ "$(count "$(in_file "$u" "$DEF_DIROPS")")" -eq 1 ] || bad "$DEF_DIROPS must contain exactly one 'unconfined' identifier (its definition), found $(count "$(in_file "$u" "$DEF_DIROPS")") (0 also means the scan is blind)"
  [ "$(count "$(in_file "$u" "$SITE_UNCONF")")" -eq "$expect" ] || bad "$SITE_UNCONF must contain exactly $expect 'unconfined' identifier(s) (the call site), found $(count "$(in_file "$u" "$SITE_UNCONF")")"
  flag "'unconfined' outside its definition and call site" < <(not_in "$u" "^($DEF_DIROPS|$SITE_UNCONF):")
  flag "for_tests outside crates/*/tests/, crates/{core,adapters}/src/testing.rs and apply_token.rs" < <(not_in "$t" "^(crates/[^/]+/tests/|crates/(core|adapters)/src/testing\.rs:|$DEF_TOKEN:)")
  flag "Allowed::new outside $GUARD" < <(not_in "$H_allowednew" "^$GUARD:")
  [ "$(count "$(in_file "$H_allowednew" "$GUARD")")" -eq 1 ] || bad "$GUARD must contain exactly one Allowed::new call (decide_path_common), found $(count "$(in_file "$H_allowednew" "$GUARD")") (0 means the rule polices nothing)"

  # no other way to build the witnesses
  flag "macro_rules! body names a witness type (hides a mint site)" <<< "$H_macro"
  flag "include!/paste!/concat_idents! can hide a mint site" <<< "$H_macroinv"
  flag "'use ... as' renames a witness type" <<< "$H_useas"
  flag "type alias of a witness type" <<< "$H_typealias"
  flag "trait impl on ApplyToken/Allowed/WorktreeRemoval (From, Default, Clone, Deserialize, ...)" <<< "$H_traitimpl"
  flag "ApplyToken must not derive any trait" <<< "$H_derive_token"
  flag "Allowed/WorktreeRemoval derive policy is Debug only" <<< "$H_derive_witness"
  # a literal is allowed only inside grant/for_tests of the definition file; elsewhere `Self(..)` is some other type
  flag "ApplyToken tuple or struct literal outside grant/for_tests of apply_token.rs" < <(printf '%s\n' "$H_literal" | awk -F: -v def="$DEF_TOKEN" '$1 == def || $3 !~ /^Self/' | grep . || true)
  flag "impl ApplyToken has a constructor other than grant/for_tests" <<< "$H_ctorfn"
  flag "memory-forging primitive" < <(printf '%s\n%s\n%s\n%s\n' "$(printf '%s\n' "$H_ident" | awk -F: '$3 !~ /^(grant|unconfined|for_tests)$/')" "$H_ptrread" "$H_useptr" "$H_union" | grep -Ev -- "$FORGE_OK" | grep . || true)
  flag "pointer type or unsafe block names a witness type (a ZST can be forged from any pointer)" < <(printf '%s\n%s\n' "$H_ptrtype" "$H_unsafewitness" | grep . || true)
  flag "unsafe outside the adapters files that need it (dirops, space_meter, lock, signals, probe/; scan: btrfs_tree.rs only)" < <(not_in "$H_unsafe" "$UNSAFE_OK")
  if [ -f "$FIEMAP" ]; then
    need python3
    write_fiemap_scan "$tmp/fiemap_scan.py"
    local l
    while IFS= read -r l; do [ -n "$l" ] && bad "$FIEMAP content rule: $l"; done < <(python3 -I "$tmp/fiemap_scan.py" "$FIEMAP" | grep -v '^ok$' || true)
  fi
  local c
  for c in core cli; do
    local f=crates/$c/src/lib.rs; [ "$c" = cli ] && f=crates/cli/src/main.rs
    [ "$(count "$(in_file "$H_forbid" "$f")")" -ge 1 ] || bad "$f must carry #![forbid(unsafe_code)]"
  done
  [ "$fail" -eq 0 ] && echo "ok: token sites clean (grant and unconfined call sites: $expect each)"
  return "$fail"
}

# ---------------------------------------------------------------- selftest
fixture() {  # $1 = dir. A clean tree with one call site each (expect-sites 1).
  local d="$1"; mkdir -p "$d"/crates/{core/src,core/tests,adapters/src,cli/src/wiring} "$d/xtask/src"
  cat > "$d/crates/core/src/apply_token.rs" <<'EOF'
//! ApplyToken::grant() is mentioned in a comment and "ApplyToken::grant(" in a string: neither is a site.
pub struct ApplyToken(());
impl ApplyToken {
    pub fn grant() -> Self { ApplyToken(()) }
    #[cfg(feature = "test-support")]
    pub fn for_tests() -> Self { ApplyToken(()) }
}
EOF
  printf '#![forbid(unsafe_code)]\npub mod apply_token;\n' > "$d/crates/core/src/lib.rs"
  cat > "$d/crates/core/src/allowed.rs" <<'EOF'
#[derive(Debug)]
pub struct Allowed { path: u8 }
impl Allowed { pub(crate) fn new() -> Allowed { Allowed { path: 0 } } }
#[derive(Debug)]
pub struct WorktreeRemoval { a: Allowed }
EOF
  printf 'fn decide() -> Allowed { Allowed::new() }\n' > "$d/crates/core/src/guard.rs"
  printf 'fn t() { let _ = ApplyToken::for_tests(); }\n' > "$d/crates/core/tests/t.rs"
  cat > "$d/crates/adapters/src/dirops.rs" <<'EOF'
pub struct RealDirOps;
impl RealDirOps { pub fn unconfined(_p: &ApplyToken) -> RealDirOps { RealDirOps } }
fn raw() { unsafe { libc::sync() } }
EOF
  printf '#![forbid(unsafe_code)]\nmod wiring;\nfn main() {}\n' > "$d/crates/cli/src/main.rs"
  printf 'pub fn grant_apply() -> ApplyToken { ApplyToken::grant() }\n' > "$d/crates/cli/src/wiring/token.rs"
  printf 'pub fn ops(t: &ApplyToken) -> RealDirOps { RealDirOps::unconfined(t) }\n' > "$d/crates/cli/src/wiring/confine.rs"
  printf 'fn main() {}\n' > "$d/xtask/src/main.rs"
}

selftest() {
  need jq; find_sg >/dev/null || { echo "FAIL: ast-grep not found"; return 2; }
  local rc=0 out; t=$(mktemp -d); trap 'rm -rf "$t"' EXIT
  fixture "$t/clean"
  if out=$("$SELF" --expect-sites 1 "$t/clean" 2>&1); then echo "ok: clean tree passes (expect-sites 1): $out"; else echo "SELFTEST BAD: clean tree failed: $out"; rc=1; fi
  # early stage: no call sites yet
  cp -r "$t/clean" "$t/early"; rm "$t/early/crates/cli/src/wiring/token.rs" "$t/early/crates/cli/src/wiring/confine.rs"
  if out=$("$SELF" --expect-sites 0 "$t/early" 2>&1); then echo "ok: early tree (no call sites) passes with --expect-sites 0"; else echo "SELFTEST BAD: early tree failed: $out"; rc=1; fi
  if out=$("$SELF" --expect-sites 1 "$t/early" 2>&1); then echo "SELFTEST BAD: early tree passed with --expect-sites 1"; rc=1; else echo "ok: early tree fails with --expect-sites 1 (found 0): $(printf '%s' "$out" | head -1)"; fi
  if out=$("$SELF" --expect-sites 0 "$t/clean" 2>&1); then echo "SELFTEST BAD: tree with call sites passed with --expect-sites 0"; rc=1; else echo "ok: tree with call sites fails with --expect-sites 0"; fi
  if "$SELF" "$t/clean" >/dev/null 2>&1; then echo "SELFTEST BAD: omitted --expect-sites accepted"; rc=1; else echo "ok: --expect-sites is mandatory"; fi
  # positive controls that must NOT trip
  cp -r "$t/clean" "$t/pos"
  printf '// ApplyToken::grant() in a comment\nfn s() { let _ = "ApplyToken::grant(); //"; }\n' >> "$t/pos/crates/adapters/src/dirops.rs"
  if "$SELF" --expect-sites 1 "$t/pos" >/dev/null 2>&1; then echo "ok: grant named only in a comment or a string is not a site"; else echo "SELFTEST BAD: comment/string false positive"; rc=1; fi
  # negatives: name | file to append to (or replace) | text
  declare -A WANT=(
    [allowed-new-in-targets-dir]="Allowed::new outside"
    [allowed-new-twice-in-guard]="exactly one Allowed::new"
    [allowed-new-missing]="exactly one Allowed::new"
    [macro-arg]="macro_rules! body names a witness|'grant' outside"
    [use-as]="'use ... as' renames|'grant' outside"
    [type-alias]="type alias of a witness|'grant' outside"
    [fn-pointer]="'grant' outside"
    [multi-line]="'grant' outside"
    [string-slashes]="'grant' outside"
    [xtask]="'grant' outside"
    [self-wrapper]="exactly one 'grant' identifier"
    [second-grant-adapters]="'grant' outside"
    [grant-in-core-tests]="'grant' outside"
    [grant-renamed]="must contain exactly 1 'grant'"
    [second-unconfined]="'unconfined' outside"
    [unconfined-alias]="'use ... as' renames|'unconfined' outside"
    [impl-default]="trait impl on"
    [impl-from]="trait impl on"
    [derive-token]="ApplyToken must not derive"
    [literal-cli]="ApplyToken tuple or struct literal"
    [ctor-sneaky]="constructor other than grant/for_tests"
    [transmute]="memory-forging primitive"
    [unsafe-cli]="unsafe outside"
    [no-forbid-core]="must carry #!\[forbid"
    [for-tests-cli]="for_tests outside"
    [for-tests-inline-mod]="for_tests outside"
    [allowed-new-elsewhere]="Allowed::new outside"
    [allowed-derive-clone]="derive policy is Debug only"
    [allowed-derive]="derive policy is Debug only"
    [allowed-new-in-macro]="Allowed::new outside"
    [allowed-new-qualified]="Allowed::new outside"
    [unsafe-in-macro-arg]="unsafe outside"
    [allowed-default-impl]="trait impl on"
    [include-macro]="include!/paste!"
    [ptr-read]="memory-forging primitive"
    [r-hash-grant]="'grant' outside"
    [r-hash-unconfined]="'unconfined' outside"
    [r-hash-for-tests]="for_tests outside"
    [r-hash-transmute]="memory-forging primitive"
    [r-hash-allowed-new]="Allowed::new outside"
    [impl-scoped-default]="trait impl on"
    [impl-scoped-clone]="trait impl on"
    [impl-ref-clone]="trait impl on"
    [inherent-scoped-ctor]="constructor other than grant/for_tests"
    [free-fn-in-def]="literal outside grant/for_tests"
    [nested-fn-in-grant]="literal outside grant/for_tests"
    [zst-dangling]="memory-forging primitive"
    [zst-ptrcast]="names a witness type"
    [zst-ptr-safe-cast]="names a witness type"
    [zst-ptr-glob-use]="memory-forging primitive"
    [zst-union]="memory-forging primitive"
    [zst-wrong-file]="unsafe outside the adapters files"
    [scan-second-unsafe]="unsafe outside the adapters files"
    [scan-forge-other-file]="memory-forging primitive"
    [scan-transmute-in-btrfs-tree]="memory-forging primitive"
  )
  expect_fail() {  # $1 name, $2 dir, $3 sites; the failure must carry the diagnostic WANT[name], not a tool error
    local out; out=$("$SELF" --expect-sites "${3:-1}" "$2" 2>&1)
    if [ $? -eq 0 ]; then echo "SELFTEST BAD: evasion '$1' passed"; rc=1
    elif printf '%s' "$out" | grep -Eq 'ast-grep reported an error|did not run|Cannot parse rule'; then echo "SELFTEST BAD: '$1' failed because the tool broke, not for its own reason"; rc=1
    elif ! printf '%s' "$out" | grep -Eq -- "${WANT[$1]}"; then echo "SELFTEST BAD: '$1' failed for the wrong reason: $(printf '%s' "$out" | head -2)"; rc=1
    else echo "ok: evasion '$1' fails: $(printf '%s' "$out" | grep -E -m1 -- "${WANT[$1]}" | cut -c1-140)"; fi
  }
  neg() {  # $1 name, $2 file, $3 text, $4 mode(append|replace)
    local d="$t/n-$1"; cp -r "$t/clean" "$d"
    if [ "${4:-append}" = replace ]; then printf '%s\n' "$3" > "$d/$2"; else mkdir -p "$(dirname "$d/$2")"; printf '%s\n' "$3" >> "$d/$2"; fi
    expect_fail "$1" "$d"
  }
  neg macro-arg crates/adapters/src/x.rs 'macro_rules! tok { ($f:ident) => { disk_cleanup_core::ApplyToken::$f() } }
fn a() { let _ = tok!(grant); }'
  neg use-as crates/adapters/src/x.rs 'use disk_cleanup_core::ApplyToken as Tok;
fn a() { let _ = Tok::grant(); }'
  neg type-alias crates/adapters/src/x.rs 'type T = disk_cleanup_core::ApplyToken;
fn a() { let _ = <T>::grant(); }'
  neg fn-pointer crates/adapters/src/x.rs 'fn a() { let f = ApplyToken::grant; let _ = f(); }'
  neg multi-line crates/adapters/src/x.rs 'fn a() { let _ = ApplyToken
        ::grant(); }'
  neg string-slashes crates/adapters/src/x.rs 'fn a() { let u = "http://x"; let _ = ApplyToken::grant(); }'
  neg xtask xtask/src/main.rs 'fn main() { let _ = ApplyToken::grant(); }'
  neg self-wrapper crates/core/src/apply_token.rs 'impl ApplyToken { pub fn sneaky() -> Self { Self::grant() } }'
  neg second-grant-adapters crates/adapters/src/x.rs 'fn a() { let _ = ApplyToken::grant(); }'
  neg grant-in-core-tests crates/core/tests/x.rs 'fn a() { let _ = ApplyToken::grant(); }'
  neg grant-renamed crates/cli/src/wiring/token.rs 'pub fn grant_apply() -> ApplyToken { ApplyToken::new() }' replace
  neg second-unconfined crates/adapters/src/x.rs 'fn a(t: &ApplyToken) { let _ = RealDirOps::unconfined(t); }'
  neg unconfined-alias crates/cli/src/x.rs 'use adapters::RealDirOps as R;
fn a(t: &ApplyToken) { let f = R::unconfined; }'
  neg impl-default crates/core/src/apply_token.rs 'impl Default for ApplyToken { fn default() -> Self { ApplyToken(()) } }'
  neg impl-from crates/core/src/apply_token.rs 'impl<T> From<T> for ApplyToken { fn from(_: T) -> Self { ApplyToken(()) } }'
  # derive on ApplyToken itself:
  local d="$t/n-derive-token"; cp -r "$t/clean" "$d"; sed -i 's/^pub struct ApplyToken/#[derive(Clone, Deserialize)]\npub struct ApplyToken/' "$d/crates/core/src/apply_token.rs"
  expect_fail derive-token "$d"
  neg literal-cli crates/cli/src/x.rs 'fn a() { let _ = ApplyToken(()); }'
  neg ctor-sneaky crates/core/src/apply_token.rs 'impl ApplyToken { pub fn sneaky() -> Self { Self(()) } }'
  neg transmute crates/cli/src/x.rs 'fn a() { let _: ApplyToken = std::mem::transmute(()); }'
  neg unsafe-cli crates/cli/src/x.rs 'fn a() { unsafe { } }'
  neg no-forbid-core crates/core/src/lib.rs 'pub mod apply_token;' replace
  neg for-tests-cli crates/cli/src/main.rs 'fn a() { let _ = ApplyToken::for_tests(); }'
  neg for-tests-inline-mod crates/core/src/guard.rs '#[cfg(test)] mod t { fn a() { let _ = ApplyToken::for_tests(); } }'
  neg allowed-new-twice-in-guard crates/core/src/guard.rs 'fn d2() -> Allowed { Allowed::new() }'
  neg allowed-new-missing crates/core/src/guard.rs 'fn decide() {}' replace
  neg allowed-new-in-targets-dir crates/core/src/targets/x.rs 'fn a() -> Allowed { Allowed::new() }'
  neg allowed-new-elsewhere crates/core/src/targets.rs 'fn a() -> Allowed { Allowed::new() }'
  d="$t/n-allowed-derive-clone"; cp -r "$t/clean" "$d"; sed -i 's/^pub struct WorktreeRemoval/#[derive(Clone)] pub struct WorktreeRemoval/' "$d/crates/core/src/allowed.rs"
  expect_fail allowed-derive-clone "$d"
  d="$t/n-allowed-derive"; cp -r "$t/clean" "$d"; sed -i '0,/#\[derive(Debug)\]/s//#[derive(Debug, Default, Clone)]/' "$d/crates/core/src/allowed.rs"
  expect_fail allowed-derive "$d"
  neg allowed-default-impl crates/core/src/allowed.rs 'impl Default for Allowed { fn default() -> Self { todo!() } }'
  neg allowed-new-in-macro crates/core/src/targets.rs 'fn a() { let v = vec![Allowed::new()]; }'
  neg allowed-new-qualified crates/core/src/targets.rs 'fn a() { let v = <Allowed>::new(); }'
  neg unsafe-in-macro-arg crates/cli/src/x.rs 'fn a() { m!(unsafe { 1 }); }'
  neg include-macro crates/adapters/src/x.rs 'include!("gen.txt");'
  neg ptr-read crates/cli/src/x.rs 'fn a() { let _ = std::ptr::read(p); }'
  # iteration 5: raw identifiers, scoped-path impls, free fns in the definition file, ZST forging in adapters
  neg r-hash-grant crates/adapters/src/x.rs 'fn a() { let _ = ApplyToken::r#grant(); }'
  neg r-hash-unconfined crates/adapters/src/x.rs 'fn a(t: &ApplyToken) { let _ = RealDirOps::r#unconfined(t); }'
  neg r-hash-for-tests crates/cli/src/x.rs 'fn a() { let _ = ApplyToken::r#for_tests(); }'
  neg r-hash-transmute crates/cli/src/x.rs 'fn a() { let _: ApplyToken = std::mem::r#transmute(()); }'
  neg r-hash-allowed-new crates/core/src/targets.rs 'fn a() -> Allowed { Allowed::r#new() }'
  neg impl-scoped-default crates/core/src/apply_token.rs 'impl Default for crate::apply_token::ApplyToken { fn default() -> Self { Self(()) } }'
  neg impl-scoped-clone crates/core/src/x.rs 'impl Clone for crate::ApplyToken { fn clone(&self) -> Self { todo!() } }'
  neg impl-ref-clone crates/core/src/x.rs 'impl Clone for &ApplyToken { fn clone(&self) -> Self { todo!() } }'
  neg inherent-scoped-ctor crates/core/src/apply_token.rs 'impl crate::apply_token::ApplyToken { pub fn sneaky() -> Self { Self(()) } }'
  neg free-fn-in-def crates/core/src/apply_token.rs 'pub fn free() -> ApplyToken { ApplyToken(()) }'
  neg nested-fn-in-grant crates/core/src/apply_token.rs 'pub struct ApplyToken(());
impl ApplyToken {
    pub fn grant() -> Self { fn sneaky() -> ApplyToken { ApplyToken(()) } sneaky() }
}' replace
  neg zst-dangling crates/adapters/src/dirops.rs 'fn f() -> ApplyToken { unsafe { std::ptr::NonNull::<ApplyToken>::dangling().read() } }'
  neg zst-ptrcast crates/adapters/src/dirops.rs 'fn f(u: &()) -> &ApplyToken { unsafe { &*(u as *const () as *const ApplyToken) } }'
  neg zst-ptr-safe-cast crates/adapters/src/dirops.rs 'fn f() { let p = 1usize as *const ApplyToken; let _ = p; }'
  neg zst-ptr-glob-use crates/adapters/src/dirops.rs 'use std::ptr::*;
fn f(p: *const ApplyToken) { let _ = unsafe { read(p) }; }'
  neg zst-union crates/adapters/src/dirops.rs 'union U { a: std::mem::ManuallyDrop<ApplyToken>, b: u8 }'
  neg zst-wrong-file crates/adapters/src/command.rs 'fn f() { unsafe { } }'
  # positive: `unsafe` in a listed adapters file (probe/) that does not name a witness passes
  cp -r "$t/clean" "$t/pos2"; mkdir -p "$t/pos2/crates/adapters/src/probe"; printf 'fn f() { unsafe { libc::geteuid() }; }\n' > "$t/pos2/crates/adapters/src/probe/basic.rs"
  if "$SELF" --expect-sites 1 "$t/pos2" >/dev/null 2>&1; then echo "ok: unsafe in adapters/src/probe/ that names no witness type is allowed"; else echo "SELFTEST BAD: probe/ unsafe false positive"; rc=1; fi
  # scan crate (ADR-007 decision 6): its btrfs_tree.rs unsafe (ioctl, from_raw_parts) passes; nothing wider does
  scan_unsafe='fn h(k: &K, b: &mut [u8]) { let raw = unsafe { std::slice::from_raw_parts((k as *const K).cast::<u8>(), 8) }; b[..8].copy_from_slice(raw); }
fn i(fd: i32) { let _ = unsafe { libc::ioctl(fd, 0, 0) }; }'
  cp -r "$t/clean" "$t/pos3"; mkdir -p "$t/pos3/crates/scan/src"; printf '%s\n' "$scan_unsafe" > "$t/pos3/crates/scan/src/btrfs_tree.rs"
  if "$SELF" --expect-sites 1 "$t/pos3" >/dev/null 2>&1; then echo "ok: the scan crate's btrfs_tree.rs ioctl and from_raw_parts are allowed"; else echo "SELFTEST BAD: scan btrfs_tree.rs unsafe false positive"; rc=1; fi
  neg scan-second-unsafe crates/scan/src/walker.rs 'fn a() { unsafe { } }'
  neg scan-forge-other-file crates/scan/src/walker.rs 'fn a(p: *const u8) { let _ = std::slice::from_raw_parts(p, 1); }'
  d="$t/n-scan-transmute-in-btrfs-tree"; cp -r "$t/pos3" "$d"; printf 'fn a() { let _: u64 = unsafe { std::mem::transmute(0u64) }; }\n' >> "$d/crates/scan/src/btrfs_tree.rs"
  expect_fail scan-transmute-in-btrfs-tree "$d"
  # fiemap.rs: the only new `unsafe` file; read-only by content (python selftest holds the 25 evasions)
  write_fiemap_scan "$t/fiemap_scan.py"
  if out=$(python3 -I "$t/fiemap_scan.py" --selftest "$t" 2>&1) && printf '%s' "$out" | grep -q 'failures: 0'; then echo "ok: $out"; else echo "SELFTEST BAD: fiemap content-rule controls: $out"; rc=1; fi
  fiemap_clean='use std::io;\nuse rustix::fs::{lstat, open, OFlags, Mode};\nfn f() { let _ = unsafe { libc::ioctl(3, FS_IOC_FIEMAP, 0) }; }\nconst FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;'
  cp -r "$t/clean" "$t/pos4"; printf '%b\n' "$fiemap_clean" > "$t/pos4/crates/adapters/src/fiemap.rs"
  if out=$("$SELF" --expect-sites 1 "$t/pos4" 2>&1); then echo "ok: unsafe in adapters/src/fiemap.rs that names only allowed items passes"; else echo "SELFTEST BAD: fiemap.rs false positive: $out"; rc=1; fi
  WANT[fiemap-remove-file]="fiemap.rs content rule"
  d="$t/n-fiemap-remove-file"; cp -r "$t/pos4" "$d"; printf 'fn g() { std::fs::remove_file("x"); }\n' >> "$d/crates/adapters/src/fiemap.rs"
  expect_fail fiemap-remove-file "$d"
  WANT[fiemap-second-unsafe-file]="unsafe outside the adapters files"
  d="$t/n-fiemap-second-unsafe-file"; cp -r "$t/pos4" "$d"; printf 'fn a() { unsafe { } }\n' > "$d/crates/adapters/src/fiemap2.rs"
  expect_fail fiemap-second-unsafe-file "$d"
  WANT[fiemap-write-flag]="fiemap.rs content rule"
  d="$t/n-fiemap-write-flag"; cp -r "$t/pos4" "$d"; printf 'fn g() { let _ = rustix::fs::OFlags::WRONLY; }\n' >> "$d/crates/adapters/src/fiemap.rs"
  expect_fail fiemap-write-flag "$d"
  # the real file, when present, passes its own rule
  real="$(dirname "$SELF")/../$FIEMAP"
  if [ -f "$real" ]; then
    if out=$(python3 -I "$t/fiemap_scan.py" "$real" 2>&1); then echo "ok: the real $FIEMAP passes the content rule"; else echo "SELFTEST BAD: the real $FIEMAP fails: $out"; rc=1; fi
  fi
  # fail-closed on a blind scan: a tree where the definition files are missing
  mkdir -p "$t/blind/crates/cli/src"; echo 'fn main() {}' > "$t/blind/crates/cli/src/main.rs"
  if out=$("$SELF" --expect-sites 0 "$t/blind" 2>&1); then echo "SELFTEST BAD: blind tree passed"; rc=1; else echo "ok: a tree without the definition sites fails (scan cannot be vacuous)"; fi
  return "$rc"
}

main "$@"

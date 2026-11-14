"""Macros that keep the five crates' BUILD files short and make new files join their targets by glob.

Two library variants per crate that core/adapters' `test-support` feature reaches: `name` (what ships) and
`name_ts` (built with `test-support`, the Cargo dev-dependency feature unification). Every test, test helper binary
and the binary the integration tests run is built on the `_ts` universe, so a test can never depend on a flag and a
shipped target can never carry the feature (scripts/check-release-features.sh proves it on the dist binary).
"""

load("@bazel_skylib//rules:write_file.bzl", "write_file")
load("@rules_rust//rust:defs.bzl", "rust_binary", "rust_doc_test", "rust_library", "rust_test")

LINTS = "//build:workspace_lints"

# Crates that have a `_ts` variant; a dependency on one of them becomes its `_ts` twin in the _ts universe.
TS_CRATES = [
    "//crates/core:core",
    "//crates/adapters:adapters",
    "//crates/tui:tui",
    "//crates/cli:cli",
]

MANIFEST_ENV = ":manifest_env"

def dc_manifest_env():
    """The package's `CARGO_MANIFEST_DIR` for rustc.

    rules_rust bakes `${pwd}/<package>` (the sandbox's execroot, gone by the time a test runs) into `env!`. An env
    file is applied after rules_rust's own value (process_wrapper `environment_block`), so this relative path wins:
    tests run in `<runfiles>/_main`, where `crates/<name>` holds the declared data, and the tests need no edit.
    """
    write_file(
        name = "manifest_env",
        out = "manifest_env.txt",
        content = ["CARGO_MANIFEST_DIR=" + native.package_name(), ""],
    )

def ts_deps(deps):
    """Maps a dep list into the test-support universe. Works on plain lists only (not select())."""
    return [d + "_ts" if d in TS_CRATES else d for d in deps]

def ts_select(branches):
    """`select()` whose branch values are dep lists, mapped into the test-support universe."""
    return select({k: ts_deps(v) for k, v in branches.items()})

def dc_library(name, crate_name, srcs, deps, proc_macro_deps = [], crate_features = [], ts_deps_override = None, ts_features = None, compile_data = [], with_ts = True, **kwargs):
    """rust_library `name` plus, with_ts, `name_ts` (test-support universe)."""
    common = dict(
        crate_name = crate_name,
        srcs = srcs,
        edition = "2021",
        version = "0.2.1",
        proc_macro_deps = proc_macro_deps,
        compile_data = compile_data,
        rustc_env_files = [MANIFEST_ENV],
        lint_config = LINTS,
        visibility = ["//visibility:public"],
        **kwargs
    )
    rust_library(name = name, deps = deps, crate_features = crate_features, **common)
    if with_ts:
        rust_library(
            name = name + "_ts",
            deps = ts_deps_override if ts_deps_override != None else ts_deps(deps),
            crate_features = ts_features if ts_features != None else crate_features + ["test-support"],
            **common
        )

def dc_integration_tests(crate, deps, proc_macro_deps, features = [], data = [], compile_data = [], env = {}, rustc_env = {}, per_test = {}, tags = [], size = "medium"):
    """One rust_test per tests/*.rs file (glob: a new file gets a target on its own).

    Subdirectories of tests/ (`common`, `support`, fixtures, ...) are shared sources of every test, as in Cargo
    where `mod common;` resolves beside the test file. `per_test` maps a file stem to extra rust_test kwargs
    (tags, env, data, size, ...); list values are appended, dict values merged.
    """
    shared_srcs = native.glob(["tests/*/**/*.rs"], allow_empty = True)
    shared_data = native.glob(["tests/**"], exclude = ["tests/**/*.rs", "tests/**/BUILD*"], allow_empty = True)
    for f in native.glob(["tests/*.rs"]):
        stem = f[len("tests/"):-len(".rs")]
        extra = per_test.get(stem, {})
        kw = dict(
            name = stem + "_test",
            srcs = [f] + shared_srcs,
            crate_name = stem,
            crate_root = f,
            edition = "2021",
            deps = deps,
            proc_macro_deps = proc_macro_deps,
            crate_features = features,
            data = data + shared_data + extra.get("data", []),
            compile_data = compile_data + shared_data + shared_srcs + extra.get("compile_data", []),
            env = dict({"INSTA_UPDATE": "no"}, **dict(env, **extra.get("env", {}))),
            rustc_env = dict(rustc_env, **extra.get("rustc_env", {})),
            tags = tags + extra.get("tags", []),
            size = extra.get("size", size),
            rustc_env_files = [MANIFEST_ENV],
            lint_config = LINTS,
        )
        if "linux_only" in extra:
            # `linux_only` is the reason, e.g. a pinned test file that cannot be edited; other platforms skip the
            # target (bazel lists it as incompatible).
            kw["target_compatible_with"] = select({
                "@platforms//os:linux": [],
                "//conditions:default": ["@platforms//:incompatible"],
            })
            kw["tags"] = kw["tags"] + ["linux-only"]
        if "timeout" in extra:
            kw["timeout"] = extra["timeout"]
        if "flaky" in extra:
            kw["flaky"] = extra["flaky"]
        if "env_inherit" in extra:
            kw["env_inherit"] = extra["env_inherit"]
        if "shard_count" in extra:
            kw["shard_count"] = extra["shard_count"]
        rust_test(**kw)

def dc_unit_and_doc_tests(name, crate, deps, proc_macro_deps, data = [], env = {}, tags = [], doc = True, doc_crate = None, flaky = False):
    """Unit tests (#[cfg(test)] in src/) and doc-tests of `crate` (the _ts variant, as `cargo test` builds it)."""
    rust_test(
        name = name + "_unit_test",
        crate = crate,
        deps = deps,
        proc_macro_deps = proc_macro_deps,
        data = data,
        env = env,
        tags = tags,
        flaky = flaky,
        rustc_env_files = [MANIFEST_ENV],
        lint_config = LINTS,
    )
    if doc:
        rust_doc_test(name = name + "_doc_test", crate = doc_crate or crate, deps = deps, tags = tags)

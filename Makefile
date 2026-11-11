.PHONY: gate safety mutants install-gated bazel-build bazel-test lint dist sync

# Bazel is the build, test and lint system (ADR-008); scripts/bazel.sh is bazelisk with a private output base and
# disk cache (DC_BAZEL_OUTPUT_ROOT, DC_BAZEL_DISK_CACHE, DC_BAZEL_JOBS). Cargo stays for `cargo publish`,
# `make mutants`, the Cargo/Bazel drift leg of the gate and the tests that build release profiles with cargo.
BAZEL ?= scripts/bazel.sh

# The safety_ tests that need a deleting build (Gate B on) are compiled only with the gate's registry; without it
# `make safety` would skip them silently. Raise this with the active gate (gate-c, gate-d).
SAFETY_CONFIG ?= --config=gate-d

bazel-build:
	$(BAZEL) build //...

bazel-test:
	$(BAZEL) test //...

lint:
	$(BAZEL) build --config=lint //...

# The shipped binary as the release archives build it.
dist:
	$(BAZEL) build --config=dist //crates/cli:disk-cleanup

sync:
	scripts/check-bazel-cargo-sync.sh

# make gate GATE=<A-F>: every required check for that gate plus the Bazel build, lint (rustfmt, clippy -D warnings)
# and test with the gate's registry, the cargo-only tests and a Cargo release build.
gate:
	@test -n "$(GATE)" || { echo "usage: make gate GATE=<A-F>"; exit 64; }
	scripts/gate.sh $(GATE)

# Checks that ci/safety lists and the safety_ tests agree (Story 2.3.5b), then runs every test whose name
# starts with safety_ on both profiles (fastbuild and opt, with the active gate's registry), printing the names.
safety:
	scripts/check-safety-manifest.sh
	$(BAZEL) test //... $(SAFETY_CONFIG) --test_filter=safety_ --test_output=all --test_arg=--nocapture
	$(BAZEL) test //... $(SAFETY_CONFIG) -c opt --test_filter=safety_ --test_output=all --test_arg=--nocapture

# Mutation testing over the files listed in ci/mutants.d/*.files (one file per owning story).
mutants:
	@command -v cargo-mutants >/dev/null || { echo "cargo-mutants is not installed (cargo install cargo-mutants)"; exit 1; }
	@files=$$(cat ci/mutants.d/*.files 2>/dev/null | grep -v '^#' | sed '/^$$/d'); \
	test -n "$$files" || { echo "no files listed in ci/mutants.d/*.files"; exit 1; }; \
	args=""; for f in $$files; do args="$$args --file $$f"; done; \
	env -u CARGO_TARGET_DIR cargo mutants --no-shuffle -j 3 $$args

# Copies the gated binary and its stamp only if the stamp's hash equals the hash of the file copied. The binary is the
# Bazel `-c opt --config=gate-X` build that `make gate` copied beside the stamp (Bazel reuses one output path for
# every gate, so that copy, not bazel-bin, is what the stamp names).
# The final `disk-cleanup status` creates the two 1 MiB reserve files (audit.reserve, last_plan.reserve) in the state
# dir while the disk still has room; its exit code (1 warn, 2 crit) is the verdict, not a failure of the install.
install-gated:
	@test -n "$(GATE)" || { echo "usage: make install-gated GATE=<A-F>"; exit 64; }
	@dir=$$(git rev-parse --git-dir)/gate-reports; stamp=$$dir/stamp-$(GATE).json; \
	test -f "$$stamp" || { echo "no stamp for gate $(GATE): run make gate GATE=$(GATE)"; exit 1; }; \
	bin=$$dir/bin-$(GATE); test -f "$$bin" || { echo "no gated binary $$bin: run make gate GATE=$(GATE)"; exit 1; }; \
	want=$$(jq -r .binary_sha256 "$$stamp"); have=$$(sha256sum $$bin | cut -d' ' -f1); \
	test "$$want" = "$$have" || { echo "stamp hash $$want does not match $$bin ($$have)"; exit 1; }; \
	state=$${XDG_STATE_HOME:-$$HOME/.local/state}/disk-cleanup; \
	mkdir -p "$$state" "$$HOME/.cargo/bin"; \
	install -m 0755 $$bin "$$HOME/.cargo/bin/disk-cleanup"; \
	cp "$$stamp" "$$state/gate-stamp.json"; \
	"$$HOME/.cargo/bin/disk-cleanup" status >/dev/null 2>&1 || true; \
	echo "installed $$HOME/.cargo/bin/disk-cleanup (stamp gate $(GATE))"

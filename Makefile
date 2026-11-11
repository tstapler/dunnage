.PHONY: gate safety mutants install-gated

# The safety_ tests that need a deleting build (Gate B on) are compiled only with the gate's feature; without it
# `make safety` would skip them silently. Raise this with the active gate (gate-c, gate-d).
SAFETY_FEATURES ?= --features disk-cleanup/gate-b

# make gate GATE=<A-E>: every required check for that gate plus fmt, clippy, test and a release build.
gate:
	@test -n "$(GATE)" || { echo "usage: make gate GATE=<A-E>"; exit 64; }
	scripts/gate.sh $(GATE)

# Checks that ci/safety lists and the safety_ tests agree (Story 2.3.5b), then runs every test whose name
# starts with safety_ on both profiles (with the active gate's feature), printing the names.
safety:
	scripts/check-safety-manifest.sh
	cargo test --workspace $(SAFETY_FEATURES) safety_ -- --nocapture
	cargo test --workspace --release $(SAFETY_FEATURES) safety_ -- --nocapture

# Mutation testing over the files listed in ci/mutants.d/*.files (one file per owning story).
mutants:
	@command -v cargo-mutants >/dev/null || { echo "cargo-mutants is not installed (cargo install cargo-mutants)"; exit 1; }
	@files=$$(cat ci/mutants.d/*.files 2>/dev/null | grep -v '^#' | sed '/^$$/d'); \
	test -n "$$files" || { echo "no files listed in ci/mutants.d/*.files"; exit 1; }; \
	args=""; for f in $$files; do args="$$args --file $$f"; done; \
	env -u CARGO_TARGET_DIR cargo mutants --no-shuffle -j 3 $$args

# Copies the gated binary and its stamp only if the stamp's hash equals the hash of the file copied.
# Task 2.2.1b adds the final `disk-cleanup status` step (reserve files) once the binary has that command.
install-gated:
	@test -n "$(GATE)" || { echo "usage: make install-gated GATE=<A-E>"; exit 64; }
	@stamp=$$(git rev-parse --git-dir)/gate-reports/stamp-$(GATE).json; \
	test -f "$$stamp" || { echo "no stamp for gate $(GATE): run make gate GATE=$(GATE)"; exit 1; }; \
	bin=target/release/disk-cleanup; \
	want=$$(jq -r .binary_sha256 "$$stamp"); have=$$(sha256sum $$bin | cut -d' ' -f1); \
	test "$$want" = "$$have" || { echo "stamp hash $$want does not match $$bin ($$have)"; exit 1; }; \
	state=$${XDG_STATE_HOME:-$$HOME/.local/state}/disk-cleanup; \
	mkdir -p "$$state" "$$HOME/.cargo/bin"; \
	install -m 0755 $$bin "$$HOME/.cargo/bin/disk-cleanup"; \
	cp "$$stamp" "$$state/gate-stamp.json"; \
	echo "installed $$HOME/.cargo/bin/disk-cleanup (stamp gate $(GATE))"

# confed — common development tasks.
#
# `make` on its own lists the targets. `make ci` runs everything the GitHub
# Actions workflow runs, so a green `make ci` means a green pipeline.

CARGO ?= cargo
MSRV  ?= 1.85.0
BIN   ?= confed

# Clippy denies warnings the way CI does (RUSTFLAGS in .github/workflows/ci.yml).
CLIPPY_FLAGS ?= -D warnings

.DEFAULT_GOAL := help

# ---------------------------------------------------------------- building --

## build: compile the workspace in debug mode
build:
	$(CARGO) build --workspace

## release: compile an optimized binary
release:
	$(CARGO) build --workspace --release

## install: install confed from this checkout
install:
	$(CARGO) install --path crates/$(BIN) --locked

## run: run confed; pass arguments with ARGS, e.g. `make run ARGS="status --json"`
run:
	$(CARGO) run --bin $(BIN) -- $(ARGS)

# ----------------------------------------------------------------- testing --

## test: run the whole test suite
test:
	$(CARGO) test --workspace

## test-one: run tests matching NAME, e.g. `make test-one NAME=reanchor`
test-one:
	@test -n "$(NAME)" || { echo "usage: make test-one NAME=<filter>"; exit 2; }
	$(CARGO) test --workspace -- --nocapture $(NAME)

## test-sync: the end-to-end sync scenarios, run against both Confluence flavors
test-sync:
	$(CARGO) test -p confed-core --test sync_scenarios

## test-cli: drive the real binary against a mocked Confluence server
test-cli:
	$(CARGO) test -p confed --test cli

# ------------------------------------------------------------- correctness --

## fmt: format the workspace
fmt:
	$(CARGO) fmt --all

## fmt-check: fail if anything is unformatted (what CI runs)
fmt-check:
	$(CARGO) fmt --all --check

## lint: clippy over every target, warnings denied
lint:
	$(CARGO) clippy --workspace --all-targets -- $(CLIPPY_FLAGS)

## lint-fix: apply the clippy suggestions that can be applied automatically
lint-fix:
	$(CARGO) clippy --fix --workspace --all-targets --allow-dirty -- $(CLIPPY_FLAGS)

## doc: build the API documentation
doc:
	$(CARGO) doc --workspace --no-deps

## doc-open: build the API documentation and open it
doc-open:
	$(CARGO) doc --workspace --no-deps --open

## changelog: copy CHANGELOG.md into the confed crate, which compiles it in
changelog:
	cp CHANGELOG.md crates/confed/CHANGELOG.md

## msrv: check the workspace still builds on the minimum supported Rust version
msrv:
	@rustup toolchain list | grep -q '^$(MSRV)' \
		|| { echo "installing Rust $(MSRV)…"; rustup toolchain install $(MSRV) --profile minimal; }
	$(CARGO) +$(MSRV) check --workspace --all-targets

## audit: check dependencies for security advisories
audit:
	@command -v cargo-audit >/dev/null \
		|| { echo "cargo-audit is not installed: cargo install cargo-audit"; exit 127; }
	$(CARGO) audit

## ci: everything the CI pipeline runs
ci: fmt-check lint doc test
	@echo 'ok — CI also runs "make msrv" and "make audit"'

# ----------------------------------------------------------------- tidying --

## clean: remove build artifacts
clean:
	$(CARGO) clean

## clean-cache: drop the incremental cache only, keeping compiled dependencies
clean-cache:
	rm -rf target/debug/incremental target/release/incremental

## update: update dependencies within their declared version ranges
update:
	$(CARGO) update

# -------------------------------------------------------------------- help --

## help: list the available targets
help:
	@echo "confed — development tasks"
	@echo
	@sed -n 's/^## \([a-z-]*\): \(.*\)/  \1\t\2/p' $(MAKEFILE_LIST) | expand -t 16
	@echo
	@echo "Variables: CARGO=$(CARGO)  MSRV=$(MSRV)  ARGS (for make run)  NAME (for make test-one)"

.PHONY: build release install run test test-one test-sync test-cli \
        fmt fmt-check lint lint-fix doc doc-open changelog msrv audit ci \
        clean clean-cache update help

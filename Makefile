# gitrs-mcp developer tasks. Run `make` or `make help` to list targets.

BIN      := gitrs-mcp
REPO     ?= .
FILE     ?= README.md
ARGS     ?=
LOG      ?= info
DEBUG_BIN   := target/debug/$(BIN)
RELEASE_BIN := target/release/$(BIN)

.DEFAULT_GOAL := help
.PHONY: help build release run run-write test test-verbose fmt fmt-check clippy check \
        coverage coverage-html smoke bench stress fuzz inspector install uninstall mcp-add doc deps outdated clean

help: ## Show this help
	@awk 'BEGIN {FS = ":.*## "} /^[a-zA-Z_-]+:.*## / {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo
	@echo "  Variables: REPO=<path> (default .)  ARGS=<extra server args>  LOG=<level> (default info)"
	@echo "             FILE=<tracked file in REPO> for smoke (default README.md)  RUNS=<n> for bench (default 5)"
	@echo "             FUZZ_TIME=<seconds> per fuzz target (default 60)"

## --- Build ---------------------------------------------------------------

build: ## Debug build
	cargo build

release: ## Optimized release build
	cargo build --release

## --- Run -----------------------------------------------------------------

run: build ## Run the server read-only on REPO over stdio
	GITRS_LOG=$(LOG) $(DEBUG_BIN) --repo $(REPO) $(ARGS)

run-write: build ## Run the server with --allow-write on REPO
	GITRS_LOG=$(LOG) $(DEBUG_BIN) --repo $(REPO) --allow-write $(ARGS)

smoke: build ## End-to-end MCP round trip (initialize, tools/list, every tool) on REPO
	scripts/smoke.sh $(DEBUG_BIN) $(REPO) $(FILE)

RUNS ?= 5

bench: release ## Time tool calls against git on REPO (use a big repo such as git.git), RUNS each
	scripts/bench.py $(RELEASE_BIN) $(REPO) $(RUNS)

stress: release ## Heaviest calls at their largest limits on REPO; fails if a response isn't bounded
	scripts/bench.py --stress $(RELEASE_BIN) $(REPO)

FUZZ_TIME ?= 60
FUZZ_TARGETS := paths resource_uris dates revisions

fuzz: ## Run each cargo-fuzz target for FUZZ_TIME seconds (needs nightly and cargo-fuzz)
	@for target in $(FUZZ_TARGETS); do \
		echo "== $$target"; \
		cargo +nightly fuzz run $$target -- -max_total_time=$(FUZZ_TIME) || exit 1; \
	done

inspector: build ## Open the MCP Inspector UI against the server (needs npx)
	npx -y @modelcontextprotocol/inspector $(abspath $(DEBUG_BIN)) --repo $(abspath $(REPO)) $(ARGS)

## --- Quality -------------------------------------------------------------

test: ## Run unit tests
	cargo test

test-verbose: ## Run tests showing output
	cargo test -- --nocapture

fmt: ## Format code
	cargo fmt

fmt-check: ## Check formatting without changing files
	cargo fmt -- --check

clippy: ## Lint with warnings as errors
	cargo clippy --all-targets -- -D warnings

check: fmt-check clippy test smoke ## Everything CI should run

COVERAGE_MIN ?= 80

coverage: ## Test coverage summary; fails below COVERAGE_MIN% lines (needs cargo-llvm-cov)
	cargo llvm-cov --fail-under-lines $(COVERAGE_MIN)

coverage-html: ## Line-by-line HTML coverage report, opened in the browser
	cargo llvm-cov --html --open

doc: ## Build and open API docs
	cargo doc --no-deps --open

## --- Install -------------------------------------------------------------

install: ## Install the binary to ~/.cargo/bin
	cargo install --path . --locked

uninstall: ## Remove the installed binary
	cargo uninstall $(BIN)

mcp-add: release ## Register the release build with Claude Code for REPO
	claude mcp add gitrs -- $(abspath $(RELEASE_BIN)) --repo $(abspath $(REPO)) $(ARGS)

## --- Dependencies & cleanup ----------------------------------------------

deps: ## Show the dependency tree (top level)
	cargo tree --depth 1

outdated: ## List outdated dependencies (needs cargo-outdated)
	cargo outdated --root-deps-only

clean: ## Remove build artifacts
	cargo clean

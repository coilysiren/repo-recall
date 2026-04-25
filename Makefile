.DEFAULT_GOAL := help

.PHONY: help install run watch build release test fmt fmt-check lint check ci clean

# Config ---------------------------------------------------------------------
# cwd defaults to $REPO_RECALL_CWD if exported, else $(CURDIR). Lets callers
# do `REPO_RECALL_CWD=$(pwd) make -C repo-recall run` from a parent dir.
cwd   ?= $(or $(REPO_RECALL_CWD),$(CURDIR))
port  ?= $(or $(REPO_RECALL_PORT),7777)
depth ?= $(or $(REPO_RECALL_DEPTH),4)

help: ## Show this help
	@perl -nle'print $& if m{^[a-zA-Z_-]+:.*?## .*$$}' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-18s\033[0m %s\n", $$1, $$2}'

install: ## Install dev tooling (cargo-watch, pre-commit hooks)
	cargo install cargo-watch --locked
	@command -v pre-commit >/dev/null || pip install --user pre-commit
	pre-commit install

run: ## Run the server against the current directory
	REPO_RECALL_CWD=$(cwd) REPO_RECALL_PORT=$(port) REPO_RECALL_DEPTH=$(depth) cargo run

watch: ## Run under cargo-watch (rebuild + browser livereload on save)
	REPO_RECALL_CWD=$(cwd) REPO_RECALL_PORT=$(port) REPO_RECALL_DEPTH=$(depth) \
		cargo watch -w src -w Cargo.toml -w static -x run

build: ## cargo build (dev)
	cargo build

release: ## cargo build --release
	cargo build --release

test: ## Run cargo test (unit + integration)
	cargo test --color always

fmt: ## Format everything with rustfmt
	cargo fmt --all

fmt-check: ## Check formatting; non-zero exit if anything would change
	cargo fmt --all --check

lint: ## Run clippy with warnings-as-errors
	cargo clippy --all-targets --all-features -- -D warnings

check: ## Fast type-check
	cargo check --all-targets

ci: fmt-check lint check test ## Everything CI runs, in order. Fail fast.

clean: ## Remove target/ and the SQLite cache
	cargo clean
	rm -f $${TMPDIR:-/tmp}/repo-recall.sqlite $${TMPDIR:-/tmp}/repo-recall.sqlite-wal $${TMPDIR:-/tmp}/repo-recall.sqlite-shm

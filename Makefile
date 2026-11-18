# make        list the targets
# make run    build and start the GUI (debug build, backtraces on)
.DEFAULT_GOAL := help
export RUST_BACKTRACE ?= 1
ARGS ?=

help: ## list the targets
	@grep -E '^[a-z-]+:.*##' $(MAKEFILE_LIST) | sed -E 's/:.*## /\t/' | expand -t 12

run: ## build and start the GUI (debug)
	cargo run -p greencode-app

cli: ## run the native TUI, or headless with ARGS='-p "hello"'
	cargo run -p greencode-core --bin greencode-cli -- $(ARGS)

debug: ## start the GUI under lldb (type `run`; `bt` after a crash)
	cargo build -p greencode-app
	rust-lldb target/debug/greencode

test: ## run every test; one test: make test ARGS=name
	cargo test $(ARGS)

check: ## fast compile check without building binaries
	cargo check --workspace --all-targets

release: ## optimized binaries in target/release (greencode, greencode-cli)
	cargo build --release
	@ls -lh target/release/greencode target/release/greencode-cli

clean: ## delete build output
	cargo clean

.PHONY: help run cli debug test check release clean

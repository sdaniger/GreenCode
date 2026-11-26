# make        list the targets
# make run    build and start the GUI (debug build, backtraces on)
# make bundle assemble target/GreenCode.app (registers greencode:// links) from target/release
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

bundle: release ## assemble target/GreenCode.app so greencode:// links and `greencode-cli app` reach the GUI (macOS)
	rm -rf target/GreenCode.app
	mkdir -p target/GreenCode.app/Contents/MacOS target/GreenCode.app/Contents/Resources
	cp target/release/greencode target/release/greencode-cli target/GreenCode.app/Contents/MacOS/
	cp packaging/macos/AppIcon.icns target/GreenCode.app/Contents/Resources/
	sed "s/@VERSION@/$$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)/" packaging/macos/Info.plist > target/GreenCode.app/Contents/Info.plist
	/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f target/GreenCode.app

clean: ## delete build output
	cargo clean

.PHONY: help run cli debug test check release bundle clean

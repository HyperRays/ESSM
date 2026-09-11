SHELL := /bin/sh

MIX ?= mix
CARGO ?= cargo

WORKSPACE_ROOT := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
FINDEX_DIR := $(WORKSPACE_ROOT)/findex
RUST_CLIENT_DIR := $(WORKSPACE_ROOT)/rust_client
BACKEND_DIR := $(RUST_CLIENT_DIR)/backend
DESKTOP_DIR := $(WORKSPACE_ROOT)/desktop

.DEFAULT_GOAL := build

.PHONY: all build findex backend rust-client desktop \
	fmt check check-format check-rust check-native test verify \
	release package run clean check-tools check-platform check-mix check-cargo help

all: build

## Build the complete development stack.
build: desktop

findex: check-platform check-mix
	cd "$(FINDEX_DIR)" && MIX_ENV=dev $(MIX) compile

backend: findex
	cd "$(BACKEND_DIR)" && MIX_ENV=dev $(MIX) compile

rust-client: check-cargo
	$(CARGO) build --locked --manifest-path "$(RUST_CLIENT_DIR)/Cargo.toml"

# Cargo builds the path-dependent Rust client as part of the desktop build.
desktop: backend check-cargo
	$(CARGO) build --locked --manifest-path "$(DESKTOP_DIR)/Cargo.toml"

## Format the Elixir and Rust sources.
fmt: check-mix check-cargo
	cd "$(FINDEX_DIR)" && $(MIX) format
	cd "$(BACKEND_DIR)" && $(MIX) format
	$(CARGO) fmt --manifest-path "$(RUST_CLIENT_DIR)/Cargo.toml"
	$(CARGO) fmt --manifest-path "$(DESKTOP_DIR)/Cargo.toml"

## Check formatting, Rust lints, and native C diagnostics.
check: check-format check-rust check-native

check-format: check-mix check-cargo
	cd "$(FINDEX_DIR)" && $(MIX) format --check-formatted
	cd "$(BACKEND_DIR)" && $(MIX) format --check-formatted
	$(CARGO) fmt --check --manifest-path "$(RUST_CLIENT_DIR)/Cargo.toml"
	$(CARGO) fmt --check --manifest-path "$(DESKTOP_DIR)/Cargo.toml"

check-rust: check-cargo
	$(CARGO) clippy --locked --all-targets --manifest-path "$(RUST_CLIENT_DIR)/Cargo.toml" -- -D warnings
	$(CARGO) clippy --locked --all-targets --manifest-path "$(DESKTOP_DIR)/Cargo.toml" -- -D warnings

check-native: check-platform
	$(MAKE) -C "$(FINDEX_DIR)/native" analyze

## Exercise the core, bridge, client, and desktop test suites.
test: backend check-cargo
	cd "$(FINDEX_DIR)" && MIX_ENV=test $(MIX) test
	cd "$(BACKEND_DIR)" && MIX_ENV=test $(MIX) test
	$(CARGO) test --locked --all-targets --manifest-path "$(RUST_CLIENT_DIR)/Cargo.toml"
	$(CARGO) test --locked --doc --manifest-path "$(RUST_CLIENT_DIR)/Cargo.toml"
	$(CARGO) test --locked --all-targets --manifest-path "$(DESKTOP_DIR)/Cargo.toml"

# Keep checks ahead of tests, including when make is invoked with -j.
verify: check
	$(MAKE) -f "$(WORKSPACE_ROOT)/Makefile" test

## Produce optimized components without assembling the macOS app bundle.
release: check-tools
	cd "$(FINDEX_DIR)" && MIX_ENV=prod $(MIX) compile
	cd "$(BACKEND_DIR)" && MIX_ENV=prod $(MIX) release backend --overwrite
	$(CARGO) build --locked --release --manifest-path "$(RUST_CLIENT_DIR)/Cargo.toml"
	$(CARGO) build --locked --release --manifest-path "$(DESKTOP_DIR)/Cargo.toml"

## Assemble desktop/dist/ESSM.app.
package: check-tools
	cd "$(DESKTOP_DIR)" && ./package.sh

run: build
	cd "$(WORKSPACE_ROOT)" && "$(DESKTOP_DIR)/target/debug/essm"

clean: check-mix check-cargo
	cd "$(FINDEX_DIR)" && $(MIX) clean
	$(MAKE) -C "$(FINDEX_DIR)/native" clean
	cd "$(BACKEND_DIR)" && $(MIX) clean
	$(CARGO) clean --manifest-path "$(RUST_CLIENT_DIR)/Cargo.toml"
	$(CARGO) clean --manifest-path "$(DESKTOP_DIR)/Cargo.toml"

check-tools: check-platform check-mix check-cargo

check-mix:
	@command -v "$(MIX)" >/dev/null 2>&1 || { printf '%s\n' 'error: mix is required' >&2; exit 1; }

check-cargo:
	@command -v "$(CARGO)" >/dev/null 2>&1 || { printf '%s\n' 'error: cargo is required' >&2; exit 1; }

check-platform:
	@test "$$(uname -s)" = Darwin || { printf '%s\n' 'error: Findex requires macOS' >&2; exit 1; }

help:
	@printf '%s\n' \
		'make build        Build the engine, bridge, and desktop with its Rust client (default)' \
		'make findex       Build the native library and Elixir engine' \
		'make backend      Build the development stdio bridge and engine' \
		'make rust-client  Build the standalone Rust client library' \
		'make desktop      Build the desktop and its dependencies' \
		'make fmt          Format Elixir and Rust sources' \
		'make check        Check formatting, strict Clippy, and native C diagnostics' \
		'make check-format Check formatting without modifying source files' \
		'make check-rust   Run strict Clippy for both Rust crates' \
		'make check-native Run native C analysis and strict compiler warnings' \
		'make test         Run all suites, including Rust client documentation examples' \
		'make verify       Run check followed by test' \
		'make release      Build optimized components and the backend OTP release' \
		'make package      Assemble the self-contained macOS .app bundle' \
		'make run          Build and launch the development desktop app' \
		'make clean        Remove Mix and Cargo build outputs'

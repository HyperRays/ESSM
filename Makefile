SHELL := /bin/sh

MIX ?= mix
CARGO ?= cargo

WORKSPACE_ROOT := $(CURDIR)
FINDEX_DIR := $(WORKSPACE_ROOT)/findex
RUST_CLIENT_DIR := $(WORKSPACE_ROOT)/rust_client
BACKEND_DIR := $(RUST_CLIENT_DIR)/backend
DESKTOP_DIR := $(WORKSPACE_ROOT)/desktop

.DEFAULT_GOAL := build

.PHONY: all build findex backend rust-client desktop \
	check test verify release package run clean check-tools help

all: build

## Build the complete development stack.
build: desktop

findex: check-tools
	cd "$(FINDEX_DIR)" && MIX_ENV=dev $(MIX) compile

backend: findex
	cd "$(BACKEND_DIR)" && MIX_ENV=dev $(MIX) compile

rust-client: backend
	$(CARGO) build --manifest-path "$(RUST_CLIENT_DIR)/Cargo.toml"

desktop: rust-client
	$(CARGO) build --manifest-path "$(DESKTOP_DIR)/Cargo.toml"

## Run formatters and strict Rust lints without changing source files.
check: check-tools
	cd "$(FINDEX_DIR)" && $(MIX) format --check-formatted
	cd "$(BACKEND_DIR)" && $(MIX) format --check-formatted
	$(CARGO) fmt --check --manifest-path "$(RUST_CLIENT_DIR)/Cargo.toml"
	$(CARGO) fmt --check --manifest-path "$(DESKTOP_DIR)/Cargo.toml"
	$(CARGO) clippy --all-targets --manifest-path "$(RUST_CLIENT_DIR)/Cargo.toml" -- -D warnings
	$(CARGO) clippy --all-targets --manifest-path "$(DESKTOP_DIR)/Cargo.toml" -- -D warnings

## Exercise the core, bridge, client, and desktop test suites.
test: build
	cd "$(FINDEX_DIR)" && MIX_ENV=test $(MIX) test
	cd "$(BACKEND_DIR)" && MIX_ENV=test $(MIX) test
	$(CARGO) test --all-targets --manifest-path "$(RUST_CLIENT_DIR)/Cargo.toml"
	$(CARGO) test --all-targets --manifest-path "$(DESKTOP_DIR)/Cargo.toml"

verify: check test

## Produce optimized components without assembling the macOS app bundle.
release: check-tools
	cd "$(FINDEX_DIR)" && MIX_ENV=prod $(MIX) compile
	cd "$(BACKEND_DIR)" && MIX_ENV=prod $(MIX) release backend --overwrite
	$(CARGO) build --release --manifest-path "$(RUST_CLIENT_DIR)/Cargo.toml"
	$(CARGO) build --release --manifest-path "$(DESKTOP_DIR)/Cargo.toml"

## Assemble desktop/dist/Enclosed Space Searching Machine.app.
package: check-tools
	cd "$(DESKTOP_DIR)" && ./package.sh

run: build
	cd "$(WORKSPACE_ROOT)" && "$(DESKTOP_DIR)/target/debug/essm"

clean: check-tools
	cd "$(FINDEX_DIR)" && $(MIX) clean
	$(MAKE) -C "$(FINDEX_DIR)/native" clean
	cd "$(BACKEND_DIR)" && $(MIX) clean
	$(CARGO) clean --manifest-path "$(RUST_CLIENT_DIR)/Cargo.toml"
	$(CARGO) clean --manifest-path "$(DESKTOP_DIR)/Cargo.toml"

check-tools:
	@command -v "$(MIX)" >/dev/null 2>&1 || { printf '%s\n' 'error: mix is required' >&2; exit 1; }
	@command -v "$(CARGO)" >/dev/null 2>&1 || { printf '%s\n' 'error: cargo is required' >&2; exit 1; }
	@test "$$(uname -s)" = Darwin || { printf '%s\n' 'error: Findex requires macOS' >&2; exit 1; }

help:
	@printf '%s\n' \
		'make build    Build Findex, the bridge, Rust client, and desktop (default)' \
		'make check    Check formatting and run Clippy with warnings denied' \
		'make test     Build and run every test suite' \
		'make verify   Run check followed by test' \
		'make release  Build optimized components and the backend OTP release' \
		'make package  Assemble the self-contained macOS .app bundle' \
		'make run      Build and launch the development desktop app' \
		'make clean    Remove Mix and Cargo build outputs'

#!/bin/sh
set -eu

NATIVE_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROJECT_DIR=$(dirname -- "$NATIVE_DIR")
ERLANG_ROOT=$(erl -noshell -eval 'io:format("~s", [code:root_dir()]), halt().')
ERTS_VERSION=$(erl -noshell -eval 'io:format("~s", [erlang:system_info(version)]), halt().')
ERTS_BIN="$ERLANG_ROOT/erts-$ERTS_VERSION/bin"
ELIXIR_ROOT=$(elixir -e 'IO.write(:code.lib_dir(:elixir))')
ELIXIR_LIB_DIR=$(dirname -- "$ELIXIR_ROOT")
BEAM_USER_HOME=$(elixir -e 'IO.write(System.user_home!())')
TSAN_RUNTIME="$(xcrun clang -print-resource-dir)/lib/darwin/libclang_rt.tsan_osx_dynamic.dylib"

restore_optimized_build() {
  make -C "$NATIVE_DIR" clean all
}

trap restore_optimized_build EXIT HUP INT TERM

make -C "$NATIVE_DIR" clean all \
  CC='xcrun clang' \
  CFLAGS='-std=c11 -O1 -g -Wall -Wextra -Werror -fPIC -fno-omit-frame-pointer -fsanitize=thread' \
  LDFLAGS='-dynamiclib -undefined dynamic_lookup -fsanitize=thread'

cd "$PROJECT_DIR"

env \
  EMU=beam \
  ROOTDIR="$ERLANG_ROOT" \
  BINDIR="$ERTS_BIN" \
  PROGNAME=erl \
  DYLD_INSERT_LIBRARIES="$TSAN_RUNTIME" \
  TSAN_OPTIONS=halt_on_error=1:second_deadlock_stack=1 \
  "$ERTS_BIN/beam.smp" -- \
    -root "$ERLANG_ROOT" \
    -bindir "$ERTS_BIN" \
    -progname erl -- \
    -home "$BEAM_USER_HOME" -- \
    -noshell \
    -elixir_root "$ELIXIR_ROOT" \
    -pa "$ELIXIR_LIB_DIR"/*/ebin \
    -s elixir start_cli -- -- \
    -extra -S mix test

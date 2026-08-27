#!/bin/sh
set -eu

if [ "$#" -ne 5 ]; then
  echo "usage: $0 ROOT CONCURRENCY ITERATIONS SAMPLE_SECONDS OUTPUT" >&2
  exit 64
fi

ROOT=$1
CONCURRENCY=$2
ITERATIONS=$3
SAMPLE_SECONDS=$4
OUTPUT=$5
RUN_LOG="${OUTPUT}.run.log"

case "$CONCURRENCY:$ITERATIONS:$SAMPLE_SECONDS" in
  *[!0-9:]* | 0:* | *:0:* | *:0)
    echo "CONCURRENCY, ITERATIONS, and SAMPLE_SECONDS must be positive integers" >&2
    exit 64
    ;;
esac

MIX_ENV=profile mix run bench/profile_traversal.exs --   repeat "$ROOT" "$CONCURRENCY" "$ITERATIONS" >"$RUN_LOG" 2>&1 &
TARGET_PID=$!

cleanup() {
  if kill -0 "$TARGET_PID" 2>/dev/null; then
    kill "$TARGET_PID" 2>/dev/null || true
  fi
}

trap cleanup EXIT HUP INT TERM

sleep 2

if ! kill -0 "$TARGET_PID" 2>/dev/null; then
  cat "$RUN_LOG" >&2
  echo "profile target exited before sampling began" >&2
  exit 1
fi

/usr/bin/sample "$TARGET_PID" "$SAMPLE_SECONDS" 1 -file "$OUTPUT"
wait "$TARGET_PID"
trap - EXIT HUP INT TERM

cat "$RUN_LOG"
echo "Native sample: $OUTPUT"
echo "Traversal log: $RUN_LOG"

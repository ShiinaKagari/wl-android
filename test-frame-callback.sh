#!/usr/bin/env bash
# test-frame-callback.sh — Integration test for frame callback dispatch
# Starts wl-android, connects mock client, tests frame callback delivery
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CARGO_TARGET="${SCRIPT_DIR}/target/debug"
export XDG_RUNTIME_DIR=/tmp/wl-android-test
export WAYLAND_DISPLAY=land-0

mkdir -p "$XDG_RUNTIME_DIR/wl-android"

cleanup() {
    echo "=== Cleaning up ==="
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    rm -f "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
    rm -f "$XDG_RUNTIME_DIR/wl-android/land.sock"
    echo "Done."
}
trap cleanup EXIT

echo "=== Building wl-android ==="
cargo build --bin wl-android 2>&1 | tail -3

echo ""
echo "=== Starting wl-android ==="
"$CARGO_TARGET/wl-android" run &
SERVER_PID=$!
sleep 1

if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "FAIL: Server did not start"
    exit 1
fi
echo "Server PID: $SERVER_PID"

echo ""
echo "=== Connecting mock client ==="
"$CARGO_TARGET/mock-client" "${XDG_RUNTIME_DIR}/wl-android/land.sock" &
MOCK_PID=$!
sleep 2

echo ""
echo "=== Running frame-callback test ==="
if "$SCRIPT_DIR/test-frame-callback" "${XDG_RUNTIME_DIR}/${WAYLAND_DISPLAY}"; then
    echo ""
    echo "=== ALL TESTS PASSED ==="
else
    echo ""
    echo "=== TEST FAILED ==="
    exit 1
fi

#!/bin/bash
# M0 Build Script — Builds the M0 probe and socket smoke test binaries.
# These are standalone Linux executables targeting aarch64-linux-android.
#
# Prerequisites:
#   - Rust toolchain with aarch64-linux-android target:
#       rustup target add aarch64-linux-android
#   - Android NDK (for linker):
#       export ANDROID_NDK_HOME=/path/to/ndk
#       OR set CC_aarch64_linux_android in ~/.cargo/config.toml
#   - OR use cargo-ndk:
#       cargo install cargo-ndk
#       cargo ndk -t arm64-v8a build --release
#
# Usage:
#   ./scripts/m0-build.sh              # build with cargo-ndk (auto-detect)
#   ./scripts/m0-build.sh --manual     # build with manual cross-compilation
#   ANDROID_NDK_HOME=/path ./scripts/m0-build.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

MODE="${1:-auto}"

echo "=== M0 Build ==="
echo "Project dir: $PROJECT_DIR"
echo

build_with_cargo_ndk() {
    if ! command -v cargo-ndk &>/dev/null; then
        echo "cargo-ndk not found. Install with: cargo install cargo-ndk"
        echo "Falling back to manual mode..."
        return 1
    fi

    # Check NDK
    if [ -z "${ANDROID_NDK_HOME:-}" ]; then
        echo "ANDROID_NDK_HOME not set. Checking common locations..."
        for ndk in \
            "$HOME/Android/Sdk/ndk"/*/ \
            "$HOME/Android/Sdk/ndk/" \
            /opt/android-ndk/ \
            /usr/local/lib/android/sdk/ndk/*/; do
            if [ -d "$ndk" ] && [ -f "$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android34-clang" ]; then
                export ANDROID_NDK_HOME="$ndk"
                echo "Found NDK: $ANDROID_NDK_HOME"
                break
            fi
        done
        if [ -z "${ANDROID_NDK_HOME:-}" ]; then
            echo "Cannot find Android NDK. Set ANDROID_NDK_HOME or install NDK."
            return 1
        fi
    fi

    # Ensure the target is installed
    rustup target add aarch64-linux-android 2>/dev/null || true

    echo "Building m0-probe..."
    cargo ndk -t arm64-v8a build --release --manifest-path "$PROJECT_DIR/m0/probe/Cargo.toml" 2>&1
    echo

    echo "Building m0-socket-smoke (server)..."
    cargo ndk -t arm64-v8a build --release --manifest-path "$PROJECT_DIR/m0/socket-smoke/Cargo.toml" --bin smoke-server 2>&1
    echo "Building m0-socket-smoke (client)..."
    cargo ndk -t arm64-v8a build --release --manifest-path "$PROJECT_DIR/m0/socket-smoke/Cargo.toml" --bin smoke-client 2>&1
    echo
}

build_manual() {
    if [ -z "${ANDROID_NDK_HOME:-}" ]; then
        echo "ERROR: ANDROID_NDK_HOME must be set for manual builds."
        echo "Usage: ANDROID_NDK_HOME=/path/to/ndk ./scripts/m0-build.sh --manual"
        exit 1
    fi

    local NDK="$ANDROID_NDK_HOME"
    local TOOLCHAIN="$NDK/toolchains/llvm/prebuilt/linux-x86_64"
    local CC="$TOOLCHAIN/bin/aarch64-linux-android34-clang"
    local AR="$TOOLCHAIN/bin/llvm-ar"

    if [ ! -f "$CC" ]; then
        echo "ERROR: Cannot find $CC"
        echo "Check ANDROID_NDK_HOME and NDK version (looking for API 34 clang)"
        exit 1
    fi

    export CC_aarch64_linux_android="$CC"
    export AR_aarch64_linux_android="$AR"
    export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CC"

    rustup target add aarch64-linux-android 2>/dev/null || true

    echo "Building m0-probe (manual)..."
    cargo build --release --target aarch64-linux-android \
        --manifest-path "$PROJECT_DIR/m0/probe/Cargo.toml" 2>&1

    echo "Building m0-socket-smoke (manual)..."
    cargo build --release --target aarch64-linux-android \
        --manifest-path "$PROJECT_DIR/m0/socket-smoke/Cargo.toml" 2>&1
}

build_standalone() {
    # Build for the host (development machine) for local testing of the socket smoke test
    echo "Building m0-socket-smoke for HOST (local testing)..."
    cargo build --release --manifest-path "$PROJECT_DIR/m0/socket-smoke/Cargo.toml" 2>&1
    echo
    echo "Built:"
    find "$PROJECT_DIR/target/release" -name "smoke-*" -type f 2>/dev/null || true
}

echo "--- Android targets (aarch64-linux-android) ---"
if [ "$MODE" = "--manual" ]; then
    build_manual || {
        echo
        echo "Manual build failed. Try: cargo install cargo-ndk && ./scripts/m0-build.sh"
    }
elif [ "$MODE" = "--host" ]; then
    build_standalone
else
    build_with_cargo_ndk || build_manual
fi

echo
echo "=== Build Complete ==="
echo
echo "Output binaries:"
find "$PROJECT_DIR/target" -name "m0-probe" -o -name "smoke-server" -o -name "smoke-client" 2>/dev/null | sort | while read -r f; do
    echo "  $f"
done

echo
echo "=== Next Steps ==="
cat <<'USAGE'

1. Push probe binary to Android host and run:
   adb push target/aarch64-linux-android/release/m0-probe /data/local/tmp/
   adb shell chmod +x /data/local/tmp/m0-probe
   adb shell /data/local/tmp/m0-probe

2. Run container probe (inside Droidspaces):
   bash scripts/container-probe.sh

3. Socket smoke test (two terminals):
   # Container side (server):
   ./smoke-server
   # Or via adb shell directly on host:
   adb push target/aarch64-linux-android/release/smoke-server /data/local/tmp/
   adb push target/aarch64-linux-android/release/smoke-client /data/local/tmp/
   adb shell /data/local/tmp/smoke-server &
   sleep 1
   adb shell /data/local/tmp/smoke-client

   # For container↔host cross-boundary test, replace /tmp/m0-smoke.sock
   # with the bind-mounted path (/run/wl-android/m0-smoke.sock)

USAGE

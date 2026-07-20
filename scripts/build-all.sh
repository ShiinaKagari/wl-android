#!/bin/bash
# build-all.sh — Build all wl-android components.
# Targets: host (dev machine), Android (aarch64-linux-android), container (aarch64-unknown-linux-gnu)
#
# Usage:
#   ./scripts/build-all.sh              # build for host (dev machine, x86_64)
#   ./scripts/build-all.sh --android    # build with cargo-ndk for Android
#   ./scripts/build-all.sh --container  # cross-compile for container (aarch64-linux-gnu)
#   ./scripts/build-all.sh --all        # build all targets
#
# Prerequisites:
#   Host:        Rust toolchain (stable)
#   Android:     cargo-ndk + Android NDK (ANDROID_NDK_HOME)
#   Container:   aarch64-linux-gnu cross-compiler + rustup target aarch64-unknown-linux-gnu

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

MODE="${1:-}"

echo "=== wl-android Build ==="
echo "Project: $PROJECT_DIR"
echo

build_host() {
    echo "--- Building for host (x86_64) ---"
    cargo build --release -p wl-android
    cargo test
    echo "Host binary: target/release/wl-android"
}

build_android() {
    echo "--- Building for Android (aarch64-linux-android) ---"
    if ! command -v cargo-ndk &>/dev/null; then
        echo "cargo-ndk not found. Install: cargo install cargo-ndk"
        exit 1
    fi
    cargo +stable ndk -t arm64-v8a build --release -p wl-android || {
        echo "Android build failed (may need wayland-sys pkg-config workaround)"
        echo "See docs for cross-compilation notes."
        exit 1
    }
    find target -path "*/arm64-v8a/release/wl-android" 2>/dev/null
}

build_container() {
    echo "--- Building for container (aarch64-unknown-linux-gnu) ---"
    rustup target add aarch64-unknown-linux-gnu 2>/dev/null || true

    if ! command -v aarch64-linux-gnu-gcc &>/dev/null; then
        echo "Cross-compiler not found. Install: apt install gcc-aarch64-linux-gnu"
        echo "Or build inside the container directly."
        exit 1
    fi

    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
    cargo build --release --target aarch64-unknown-linux-gnu -p wl-android
    echo "Container binary: target/aarch64-unknown-linux-gnu/release/wl-android"
}

case "$MODE" in
    --android)  build_android ;;
    --container) build_container ;;
    --all)
        build_host
        echo
        build_android 2>/dev/null || echo "(Android build skipped — see above)"
        echo
        build_container 2>/dev/null || echo "(Container build skipped — see above)"
        ;;
    *)
        build_host
        echo
        echo "For other targets:"
        echo "  $0 --android       # Android via cargo-ndk"
        echo "  $0 --container     # Linux aarch64 for container"
        echo "  $0 --all           # All targets"
        ;;
esac

echo
echo "=== Build Complete ==="

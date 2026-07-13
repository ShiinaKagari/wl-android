#!/usr/bin/env bash
# 检查上游依赖的最新版本和兼容性
set -euo pipefail

echo "=== Cargo dependency check ==="
cargo outdated --workspace 2>/dev/null || echo "(cargo-outdated not installed, skipping)"

echo ""
echo "=== Rust toolchain ==="
rustup show

echo ""
echo "=== Wayland protocol check ==="
pkg-config --modversion wayland-server 2>/dev/null || echo "wayland-server not found (expected in container builds)"

echo ""
echo "=== Vulkan check ==="
pkg-config --modversion vulkan 2>/dev/null || dpkg -l | grep vulkan 2>/dev/null || echo "vulkan not found on host"

echo ""
echo "=== Android SDK check ==="
if [ -n "${ANDROID_HOME:-}" ]; then
    echo "ANDROID_HOME=$ANDROID_HOME"
    ls "$ANDROID_HOME/platforms/" 2>/dev/null || echo "no platforms found"
else
    echo "ANDROID_HOME not set (needed for APK builds)"
fi

echo ""
echo "=== NDK check ==="
if [ -n "${ANDROID_NDK_HOME:-}" ]; then
    echo "ANDROID_NDK_HOME=$ANDROID_NDK_HOME"
else
    echo "ANDROID_NDK_HOME not set (needed for cargo-ndk)"
fi

echo ""
echo "=== cargo-ndk check ==="
which cargo-ndk 2>/dev/null || echo "cargo-ndk not installed"

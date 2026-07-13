#!/usr/bin/env bash
# wl-android 完整构建脚本
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$SCRIPT_DIR/.."

# === 阶段 1: 容器内目标 (x86_64-linux-gnu) ===
echo "=== Building container-side targets (x86_64-unknown-linux-gnu) ==="
cargo build --target x86_64-unknown-linux-gnu --release -p land-common
cargo build --target x86_64-unknown-linux-gnu --release -p land

# 复制到输出 (可选)
mkdir -p "$PROJECT_ROOT/output/container"
cp "$PROJECT_ROOT/target/x86_64-unknown-linux-gnu/release/libland_wlroots.so" \
   "$PROJECT_ROOT/output/container/"

# === 阶段 2: Android 目标 (aarch64-linux-android) ===
echo "=== Building Android targets (aarch64-linux-android) ==="
cargo build --target aarch64-linux-android --release -p land-common
cargo build --target aarch64-linux-android --release -p landd

mkdir -p "$PROJECT_ROOT/output/android"
cp "$PROJECT_ROOT/target/aarch64-linux-android/release/landd" \
   "$PROJECT_ROOT/output/android/"

# === 阶段 3: Magisk 模块 ===
echo "=== Building Magisk module ==="
MAGISK_DIR="$PROJECT_ROOT/magisk/module"
mkdir -p "$MAGISK_DIR/system/bin"
cp "$PROJECT_ROOT/target/aarch64-linux-android/release/landd" \
   "$MAGISK_DIR/system/bin/"

# 构建 ZIP (排除 .gitkeep 等)
(cd "$MAGISK_DIR" && zip -r "$PROJECT_ROOT/output/wl-android-daemon.zip" . \
    -x "*.gitkeep" -x "*.DS_Store")

# === 阶段 4: Android App ===
echo "=== Building Android App ==="
if command -v gradle &>/dev/null; then
    (cd "$PROJECT_ROOT/land-app" && ./gradlew assembleRelease)
    cp "$PROJECT_ROOT/land-app/app/build/outputs/apk/release/app-release.apk" \
       "$PROJECT_ROOT/output/"
else
    echo "WARNING: gradle not found, skipping APK build"
fi

echo "=== Build complete ==="
echo "Output: $PROJECT_ROOT/output/"
ls -la "$PROJECT_ROOT/output/"

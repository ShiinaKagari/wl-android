#!/usr/bin/env bash
# wl-android APK 构建脚本
# 需要在有 Android SDK + NDK + 网络的环境下运行
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
OUTPUT="$PROJECT_ROOT/output"

export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/27.0.12077973}"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"

echo "=== Build land-native for arm64-v8a ==="
(
    cd "$PROJECT_ROOT"
    cargo ndk -t arm64-v8a -- build --release -p land-native
    mkdir -p "$SCRIPT_DIR/app/src/main/jniLibs/arm64-v8a"
    cp target/aarch64-linux-android/release/libland_native.so \
       "$SCRIPT_DIR/app/src/main/jniLibs/arm64-v8a/libland-native.so"
)

echo "=== Build APK ==="
(
    cd "$SCRIPT_DIR"
    ./gradlew assembleRelease
)

echo "=== Output ==="
mkdir -p "$OUTPUT"
cp "$SCRIPT_DIR/app/build/outputs/apk/release/app-release.apk" \
   "$OUTPUT/wl-android-app.apk" 2>/dev/null || true
echo "APK: $OUTPUT/wl-android-app.apk"
echo "APK size: $(ls -lh $OUTPUT/wl-android-app.apk 2>/dev/null | awk '{print $5}')"

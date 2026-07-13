#!/usr/bin/env bash
# wl-android 完整构建
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$SCRIPT_DIR/.."

echo "=== Android targets (arm64) ==="
cd "$PROJECT_ROOT"
cargo ndk -t arm64-v8a -- build --release -p land-native
mkdir -p land-app/app/src/main/jniLibs/arm64-v8a
cp target/aarch64-linux-android/release/libland_native.so \
   land-app/app/src/main/jniLibs/arm64-v8a/libland-native.so

echo "=== APK ==="
cd "$PROJECT_ROOT/land-app"
./gradlew assembleRelease

echo "=== Output ==="
mkdir -p "$PROJECT_ROOT/output"
cp app/build/outputs/apk/release/app-release-unsigned.apk \
   "$PROJECT_ROOT/output/wl-android-app.apk"
echo "APK: $PROJECT_ROOT/output/wl-android-app.apk"

# wl-android

Zero-copy transfer of Wayland compositor output from Linux containers to Android display.

---

## Architecture

```
Container (Droidspaces)
  wl-android-compositor (headless wlroots)
    ↓ DMA-BUF fd → sendmsg(SCM_RIGHTS)
    ↓ /run/land.sock or $LAND_SOCKET
    ↓ (bind mount → /data/local/tmp/land.sock)
Android Host
  socketd (Magisk daemon) → forward fd
    ↓
  land-app (Vulkan renderer)
```

**Core: DMA-BUF file descriptors only. CPU never touches pixels.**

---

## Components

| Component | Location | Role |
|-----------|----------|------|
| **wl-android-compositor** | `test-compositor/` | Nested wlroots compositor, extracts DMA-BUF fd |
| **socketd** | `magisk/daemon/` | 80-line C socket manager, bidirectional forward |
| **land-app** | `land-app/` | Android APK: Vulkan rendering + touch |
| **land-common** | `crates/land-common/` | Shared protocol definitions |
| **wl-android-socketd** | `magisk/module/` | Magisk module: starts socketd on boot + SELinux |

---

## Build

### Container side

```bash
apt install libwlroots-dev libwayland-dev libdrm-dev
cd test-compositor && make
```

### Android side

```bash
export ANDROID_HOME=~/Android/Sdk
export ANDROID_NDK_HOME=$ANDROID_HOME/ndk/27.0.12077973

cargo ndk -t arm64-v8a -- build --release -p land-native
cp target/aarch64-linux-android/release/libland_native.so \
   land-app/app/src/main/jniLibs/arm64-v8a/libland-native.so

cd land-app && ./gradlew assembleRelease
```

---

## Installation

1. Flash Magisk module → reboot (socketd starts automatically)

```bash
adb push output/wl-android-socketd.zip /sdcard/Download/
```

2. Install APK

```bash
adb install land-app/build/outputs/apk/release/app-release-unsigned.apk
```

3. Start compositor in container

```bash
wl-android-compositor &
WAYLAND_DISPLAY=wl-android-0 your-app
```

---

## Socket Path

| Environment | Default | Override |
|-------------|---------|----------|
| Container (Linux) | `/run/land.sock` | `$LAND_SOCKET` |
| Android | `/data/local/tmp/land.sock` | — |

---

## Constraints

- ❌ No modification of third-party code
- ❌ No OpenGL ES (Vulkan only)
- ❌ No custom Wayland protocol extensions
- ❌ No `system()`, `popen()`, `exec()`, `dlopen()`
- ✅ Nested compositor, universal across all compositors
- ✅ Zero copy, CPU never touches pixels

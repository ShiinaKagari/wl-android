# wl-android

Zero-copy transfer of Wayland compositor output from Linux containers (Droidspaces) to Android display, with touch input reverse injection.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│ Droidspaces Container                                            │
│                                                                  │
│  wlroots Compositor → libland_wlroots.so                         │
│       ↓ DMA-BUF fd via SCM_RIGHTS                                │
│       ↓ Unix Socket (/dev/socket/land.sock, bind mount)          │
├─────────────────────────────────────────────────────────────────┤
│ Android Host                                                     │
│                                                                  │
│  landd (Magisk daemon) → forward fd / touch events               │
│       ↓                                                          │
│  land-app (APK) → Vulkan rendering + touch capture               │
└─────────────────────────────────────────────────────────────────┘
```

**Core principle: only pass DMA-BUF file descriptors; never touch pixel data.**

---

## Components

| Component | Location | Role |
|-----------|----------|------|
| **libland_wlroots.so** | `crates/land/` | wlroots backend plugin: extracts DMA-BUF fd from `wlr_buffer`, sends via socket |
| **landd** | `crates/landd/` | Daemon: bidirectional forwarding (frames → App, touch → container) |
| **land-common** | `crates/land-common/` | Shared protocol definitions (FrameMessage, TouchMessage) |
| **land-app** | `land-app/` | Android App: Vulkan renderer + touch gesture engine + JNI bridge |
| **wl-android-socket** | `magisk/` | Magisk module: creates `/dev/socket/` directory and SELinux rules |

---

## Build

### Prerequisites

- Rust 1.85+ (stable)
- Android SDK + NDK 27+
- cargo-ndk
- wlroots >= 0.17 (container only)
- CMake 3.22+

### Container Components

```bash
# libland_wlroots.so (wlroots backend plugin)
cargo build --target x86_64-unknown-linux-gnu --release -p land

# landd (daemon, can also run in container for testing)
cargo build --target x86_64-unknown-linux-gnu --release -p landd
```

### Android Components

```bash
export ANDROID_HOME=~/Android/Sdk
export ANDROID_NDK_HOME=$ANDROID_HOME/ndk/27.0.12077973

# Cross-compile land-native library
cargo ndk -t arm64-v8a -- build --release -p land-native
cp target/aarch64-linux-android/release/libland_native.so \
   land-app/app/src/main/jniLibs/arm64-v8a/libland-native.so

# Build APK
cd land-app && ./gradlew assembleRelease
```

### All-in-One

```bash
./scripts/build-all.sh
```

---

## Installation

### Host (Android)

1. Install Magisk module

```bash
adb push output/wl-android-socket.zip /sdcard/Download/
# Magisk Manager → Modules → Install from storage → Reboot
```

2. Install APK

```bash
adb install -r land-app/output/wl-android-app.apk
```

3. Start landd

```bash
adb shell /system/bin/landd &
# Or it starts automatically when running in container
```

### Container (Droidspaces)

```bash
# Install libland_wlroots.so
sudo cp target/x86_64-unknown-linux-gnu/release/libland_wlroots.so \
      /usr/lib/wlroots/

# Run nested compositor (or configure wlroots compositor to load plugin)
export WAYLAND_DISPLAY=wl-android-0
./test-compositor/wl-android-compositor
```

---

## Compositor Integration

Run `test-compositor/wl-android-compositor`; Wayland clients connect to its socket:

```bash
wl-android-compositor &
WAYLAND_DISPLAY=wl-android-0 your-app &
```

### Latency

End-to-end zero copy, < 3ms.

---

## Performance Constraints

| Metric | Limit |
|--------|-------|
| CPU touching pixels | Forbidden |
| surface commit → socket send | < 500µs |
| land memory | < 4MB |
| landd memory | < 2MB |
| land-app memory | < 64MB (4K double buffer) |
| fd leaks | Zero tolerance |

See [PERFORMANCE_BOUNDARIES.md](PERFORMANCE_BOUNDARIES.md).

---

## Project Structure

```
wl-android/
├── crates/
│   ├── land/              wlroots backend plugin (libland_wlroots.so)
│   ├── landd/             Daemon (epoll bidirectional forwarding)
│   └── land-common/       Shared protocol definitions
├── land-app/
│   ├── native/            Rust: Vulkan renderer + JNI (libland-native.so)
│   └── app/               Kotlin: Activity + gesture engine + C++ bridge
├── magisk/                Magisk module (socket infrastructure)
├── test-compositor/       Nested wlroots compositor
├── docs/                  Deployment guides and scripts
├── archlinux/             Arch Linux PKGBUILD
├── scripts/               Build and check scripts
├── BOUNDARIES.md          Project boundaries and constraints
├── PERFORMANCE_BOUNDARIES.md  Performance constraints
└── INTEGRATION.md         wlroots compositor integration guide
```

---

## Constraints

- ❌ No modification of third-party code (compositors, kernel, Mesa, Wayland)
- ❌ No OpenGL ES (Vulkan only)
- ❌ No custom Wayland protocol extensions
- ❌ No `system()`, `popen()`, `exec()`, `dlopen()`
- ✅ Standard wlroots backend plugin mechanism
- ✅ Every `unsafe` block requires `SAFETY:` annotation

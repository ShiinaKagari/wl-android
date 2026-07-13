# wl-android

将 Linux 容器（Droidspaces）中 Wayland 合成器的图形输出零拷贝地传输到 Android 屏幕，并支持触摸输入反向注入。

---

## 架构

```
┌─────────────────────────────────────────────────────────────────┐
│ Droidspaces 容器                                                 │
│                                                                  │
│  wlroots 合成器 → libland_wlroots.so                             │
│       ↓ DMA-BUF fd 经过 SCM_RIGHTS 传递                          │
│       ↓ Unix Socket (/dev/socket/land.sock, bind mount)          │
├─────────────────────────────────────────────────────────────────┤
│ Android 宿主机                                                   │
│                                                                  │
│  landd (Magisk 守护) → 转发 fd / 触摸事件                        │
│       ↓                                                          │
│  land-app (APK) → Vulkan 渲染 + 触控捕获                         │
└─────────────────────────────────────────────────────────────────┘
```

**核心：只传递 DMA-BUF 文件描述符，不触碰像素数据。**

---

## 组件

| 组件 | 位置 | 职责 |
|------|------|------|
| **libland_wlroots.so** | `crates/land/` | wlroots 后端插件，提取 `wlr_buffer` 的 DMA-BUF fd 发送到 socket |
| **landd** | `crates/landd/` | 守护进程，双向转发 (帧 → App, 触摸 → 容器) |
| **land-common** | `crates/land-common/` | 共享协议定义 (FrameMessage, TouchMessage) |
| **land-app** | `land-app/` | Android App：Vulkan 渲染 + 触摸手势引擎 + JNI 桥接 |
| **wl-android-socket** | `magisk/` | Magisk 模块：创建 `/dev/socket/` 目录和 SELinux 规则 |

---

## 构建

### 前置要求

- Rust 1.85+ (stable)
- Android SDK + NDK 27+
- cargo-ndk
- wlroots >= 0.17 (仅容器内)
- CMake 3.22+

### 容器组件

```bash
# libland_wlroots.so (wlroots 后端插件)
cargo build --target x86_64-unknown-linux-gnu --release -p land

# landd (守护进程，可在容器内调试运行)
cargo build --target x86_64-unknown-linux-gnu --release -p landd
```

### Android 组件

```bash
export ANDROID_HOME=~/Android/Sdk
export ANDROID_NDK_HOME=$ANDROID_HOME/ndk/27.0.12077973

# 交叉编译 land-native 库
cargo ndk -t arm64-v8a -- build --release -p land-native
cp target/aarch64-linux-android/release/libland_native.so \
   land-app/app/src/main/jniLibs/arm64-v8a/libland-native.so

# 构建 APK
cd land-app && ./gradlew assembleRelease
```

### 全部构建

```bash
./scripts/build-all.sh
```

---

## 安装

### 宿主机 (Android)

1. 刷入 Magisk 模块

```bash
adb push output/wl-android-socket.zip /sdcard/Download/
# Magisk Manager → 模块 → 从本地安装 → 重启
```

2. 安装 APK

```bash
adb install -r land-app/output/wl-android-app.apk
```

3. 启动 landd

```bash
adb shell /system/bin/landd &
# 或在容器内运行时自动启动
```

### 容器 (Droidspaces)

```bash
# 安装 libland_wlroots.so
sudo cp target/x86_64-unknown-linux-gnu/release/libland_wlroots.so \
      /usr/lib/wlroots/

# 运行嵌套合成器 (或配置 wlroots 合成器加载插件)
export WAYLAND_DISPLAY=wl-android-0
./test-compositor/wl-android-compositor
```

---

## 合成器集成

### wlroots (原生插件)

直接加载 `libland_wlroots.so`，在 surface commit 时 inline 提取 DMA-BUF fd。

```c
#include <dlfcn.h>
void *land = dlopen("libland_wlroots.so", RTLD_NOW);
void *backend = land_create(renderer, display);
// 在 surface commit 回调中:
land_buffer_submit(backend, buffer);
```

### wlroots 嵌套合成器 (通用方案)

运行 `test-compositor/wl-android-compositor`，所有 Wayland 客户端连到该 socket：

```bash
wl-android-compositor &
WAYLAND_DISPLAY=wl-android-0 gnome-shell --nested &
WAYLAND_DISPLAY=wl-android-0 kwin_wayland --xwayland startplasma-wayland &
WAYLAND_DISPLAY=wl-android-0 your-app &
```

### 延迟对比

| 方式 | 延迟 | 说明 |
|------|------|------|
| wlroots 原生插件 | < 500µs | 合成器直接加载 `.so` |
| 嵌套合成器 | < 3ms | 类似 Gamescope |
| GNOME Nested | < 5ms | `gnome-shell --nested` |
| KWin Nested | < 5ms | `kwin_wayland KWaylandBackend` |

---

## 性能约束

| 指标 | 限制 |
|------|------|
| CPU 触碰像素 | 禁止 |
| surface commit → socket 发送 | < 500µs |
| land 内存 | < 4MB |
| landd 内存 | < 2MB |
| land-app 内存 | < 64MB (4K 双缓冲) |
| fd 泄漏 | 零容忍 |

详见 [PERFORMANCE_BOUNDARIES.md](PERFORMANCE_BOUNDARIES.md)。

---

## 项目结构

```
wl-android/
├── crates/
│   ├── land/              wlroots 后端插件 (libland_wlroots.so)
│   ├── landd/             守护进程 (epoll 双向转发)
│   └── land-common/       共享协议定义
├── land-app/
│   ├── native/            Rust: Vulkan 渲染 + JNI (libland-native.so)
│   └── app/               Kotlin: Activity + 手势引擎 + C++ bridge
├── magisk/                Magisk 模块 (socket 基础设施)
├── test-compositor/       嵌套 wlroots 合成器
├── docs/                  部署文档和脚本
├── archlinux/             Arch Linux PKGBUILD
├── scripts/               构建和检查脚本
├── BOUNDARIES.md          项目边界约束
├── PERFORMANCE_BOUNDARIES.md 性能约束
└── INTEGRATION.md         wlroots 合成器集成指南
```

---

## 约束

- ❌ 不修改任何第三方代码（合成器、内核、Mesa、Wayland）
- ❌ 不使用 OpenGL ES（仅 Vulkan）
- ❌ 不自定义 Wayland 协议扩展
- ❌ 禁止 `system()`、`popen()`、`exec()`、`dlopen()`
- ✅ 使用标准 wlroots 后端插件机制
- ✅ 所有 `unsafe` 代码必须有 `SAFETY:` 注释

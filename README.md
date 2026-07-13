# wl-android

将容器内 Wayland 合成器的图形输出零拷贝地传输到 Android 屏幕。

---

## 架构

```
容器 (Droidspaces)
  wl-android-compositor (headless wlroots)
    ↓ DMA-BUF fd → sendmsg(SCM_RIGHTS)
    ↓ /run/land.sock 或 LAND_SOCKET 环境变量
    ↓ (bind mount → /data/local/tmp/land.sock)
宿主机 (Android)
  socketd (Magisk 保活) → 转发 fd
    ↓
  land-app (Vulkan 渲染)
```

**核心：只传递 DMA-BUF 文件描述符，不触碰像素数据。**

---

## 组件

| 组件 | 位置 | 职责 |
|------|------|------|
| **wl-android-compositor** | `test-compositor/` | 嵌套 wlroots 合成器，提取 DMA-BUF fd 转发到 socket |
| **socketd** | `magisk/daemon/` | 80 行 C，管理 socket 生命周期，双向转发 |
| **land-app** | `land-app/` | Android APK：Vulkan 渲染 + 触控 |
| **land-common** | `crates/land-common/` | 共享协议定义 |
| **wl-android-socketd** | `magisk/module/` | Magisk 模块：开机启动 socketd + SELinux 规则 |

---

## 构建

### 容器侧

```bash
# 需要 wlroots >= 0.17
apt install libwlroots-dev libwayland-dev libdrm-dev
cd test-compositor && make
```

### Android 侧

```bash
export ANDROID_HOME=~/Android/Sdk
export ANDROID_NDK_HOME=$ANDROID_HOME/ndk/27.0.12077973

# 构建 native 库
cargo ndk -t arm64-v8a -- build --release -p land-native
cp target/aarch64-linux-android/release/libland_native.so \
   land-app/app/src/main/jniLibs/arm64-v8a/libland-native.so

# 构建 APK
cd land-app && ./gradlew assembleRelease
```

---

## 安装

1. 刷 Magisk 模块 → 重启 (socketd 自动启动)

```bash
adb push output/wl-android-socketd.zip /sdcard/Download/
# Magisk Manager → 模块 → 从本地安装
```

2. 安装 APK

```bash
adb install land-app/build/outputs/apk/release/app-release-unsigned.apk
```

3. 容器内启动

```bash
wl-android-compositor &
WAYLAND_DISPLAY=wl-android-0 your-app
```

---

## Socket 路径

| 环境 | 默认路径 | 覆盖 |
|------|---------|------|
| 容器 (Linux) | `/run/land.sock` | `$LAND_SOCKET` |
| 安卓 (Android) | `/data/local/tmp/land.sock` | — |

---

## 约束

- ❌ 不修改任何第三方代码
- ❌ 不使用 OpenGL ES（仅 Vulkan）
- ❌ 不自定义 Wayland 协议扩展
- ❌ 禁止 `system()`、`popen()`、`exec()`、`dlopen()`
- ✅ 嵌套合成器，所有合成器通用
- ✅ 零拷贝，CPU 不触碰像素

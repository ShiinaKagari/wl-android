# wl-android 快速开始

把容器内的 Wayland 画面输出到 Android 屏幕，全链路零拷贝。

---

## 一、获取二进制

### 宿主机侧（Android）

两个文件，从 GitHub Releases 或本地构建获取：

| 文件 | 作用 |
|------|------|
| `wl-android-daemon.zip` | Magisk 模块，开机启动 landd 守护进程 + 创建 socket |
| `wl-android-app.apk` | Android App，Vulkan 渲染 + 触摸控制 |

### 容器侧（Droidspaces）

两个文件，需在 **容器内** 构建（依赖 wlroots 原生 ARM64 库）：

| 文件 | 作用 |
|------|------|
| `libland_wlroots.so` | wlroots 后端插件，提取 DMA-BUF fd 发送到 socket |
| `wl-android-compositor` | 嵌套合成器，零修改合成器即可输出画面 |

容器内构建命令：

```bash
# 容器内
cargo build --target aarch64-unknown-linux-gnu --release -p land
sudo cp target/release/libland_wlroots.so /usr/lib/wlroots/

cd test-compositor && make
sudo cp wl-android-compositor /usr/local/bin/

# 或一键脚本
bash docs/scripts/container-build.sh
```

---

## 二、部署

### 第 1 步：安装 Magisk 模块

```bash
adb push output/wl-android-daemon.zip /sdcard/Download/
# 手机打开 Magisk Manager → 模块 → 从本地安装 → 选择 ZIP → 重启
```

重启后 `landd` 自动在后台运行，监听 `/dev/socket/land.sock`。

验证：

```bash
adb shell ls -la /dev/socket/land.sock
# srw-rw-rw- 1 root root 0 ... land.sock
```

### 第 2 步：安装 App

```bash
adb install -r output/wl-android-app.apk
```

打开 App，界面黑屏（等待容器推送画面）。

### 第 3 步：容器内启动

```bash
# 容器内
export LAND_SOCKET=/dev/socket/land.sock
wl-android-compositor
```

看到日志：

```
[compositor] WAYLAND_DISPLAY=wl-android-0
[land] plugin loaded
[land] backend ready
```

### 第 4 步：运行应用程序

```bash
# 容器内，新终端
WAYLAND_DISPLAY=wl-android-0 weston-simple-egl
```

Android 屏幕出现画面。

---

## 三、使用

### 单应用模式

```bash
WAYLAND_DISPLAY=wl-android-0 your-wayland-app
```

### 桌面模式

```bash
# GNOME
WAYLAND_DISPLAY=wl-android-0 gnome-shell --nested &

# KDE Plasma
WAYLAND_DISPLAY=wl-android-0 kwin_wayland --xwayland --exit-with-session startplasma-wayland &
```

### 触摸操作

| 手势 | 效果 |
|------|------|
| 单指拖动 | 移动/绘制 |
| 双指缩放 | 缩放 |
| 双指滑动 | 滚动 |

### 多应用（同时启动多个）

```bash
# 启动嵌套合成器（一旦运行，多个应用可以同时连）
wl-android-compositor &

# 启动两个应用
WAYLAND_DISPLAY=wl-android-0 weston-simple-egl &
WAYLAND_DISPLAY=wl-android-0 weston-smoke &
```

---

## 四、部署总览

```
宿主机 (Android 8 Elite)
  Magisk 模块 → landd 守护 (保活)
  APK → land-app (Vulkan 渲染)
               ↑ SCM_RIGHTS (DMA-BUF fd)
               ↓ TouchMessage
容器 (Droidspaces)
  libland_wlroots.so → 提取 fd
  wl-android-compositor → 嵌套合成器
```

## 五、验证链路

| 步骤 | 命令 | 预期结果 |
|------|------|----------|
| landd 运行 | `adb shell ls -la /dev/socket/land.sock` | socket 文件存在 |
| 合成器启动 | 容器内 `wl-android-compositor` | `backend ready` |
| 画面输出 | 容器内 `WAYLAND_DISPLAY=wl-android-0 weston-simple-egl` | Android 屏幕显示画面 |
| 触摸回传 | Android 屏幕触摸 | 容器内应用响应 |

---

## 文件清单

```
output/
├── wl-android-app.apk         1.3MB  (宿主机: APK)
├── wl-android-daemon.zip      210KB  (宿主机: Magisk 模块)
docs/scripts/
└── container-build.sh         (容器内: 一键构建脚本)
```

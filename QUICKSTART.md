# wl-android 快速开始

容器内 Wayland 画面零拷贝输出到 Android 屏幕。

---

## 安装

### 宿主机

```bash
adb push output/wl-android-socketd.zip /sdcard/Download/
# Magisk Manager → 模块 → 从本地安装 → 重启

adb install land-app/build/outputs/apk/release/app-release-unsigned.apk
```

重启后 socketd 自动运行，App 自动连接。

### 容器内

```bash
apt install libwlroots-dev libwayland-dev libdrm-dev

bash <(curl -s https://raw.githubusercontent.com/ShiinaKagari/wl-android/main/docs/scripts/container-build.sh)
```

## 使用

```bash
wl-android-compositor &
WAYLAND_DISPLAY=wl-android-0 weston-simple-egl
```

桌面环境：

```bash
WAYLAND_DISPLAY=wl-android-0 gnome-shell --nested &
WAYLAND_DISPLAY=wl-android-0 kwin_wayland --xwayland --exit-with-session startplasma-wayland &
```

## Socket 路径

| 环境 | 默认 | 覆盖 |
|------|------|------|
| 容器 | `/run/land.sock` | `$LAND_SOCKET` |
| 安卓 | `/data/local/tmp/land.sock` | — |

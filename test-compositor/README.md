# wl-android-compositor

无头嵌套 wlroots 合成器。**wl-android 唯一的容器侧组件。**

不依赖插件，不修改任何合成器代码。直接通过 wlroots API 提取 DMA-BUF fd 并转发到 Android。

## 构建

```bash
# 需要 wlroots >= 0.17
apt install libwlroots-dev libwayland-dev libdrm-dev

make
sudo cp wl-android-compositor /usr/local/bin/
```

## 运行

```bash
wl-android-compositor &
WAYLAND_DISPLAY=wl-android-0 weston-simple-egl
```

## 原理

```
Wayland 客户端 (KWin / GNOME / sway / 你的 App)
  ↓ wl_surface.commit
wl-android-compositor
  ↓ wlr_buffer_get_dmabuf() → dup fd
  ↓ sendmsg(SCM_RIGHTS)
landd → land-app (Android)
```

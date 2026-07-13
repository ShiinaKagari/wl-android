# KDE Plasma 完整桌面

不修改 KWin，不写插件。利用 KWin 内置的 Wayland backend 跑在我們的合成器之上。

## 启动

```bash
# 1. 启动嵌套合成器
wl-android-compositor &

# 2. 启动 KWin 作为 Wayland 客户端
export KWIN_WAYLAND_BACKEND=1
export WAYLAND_DISPLAY=wl-android-0

# 3. 启动 Plasma 桌面
startplasma-wayland
```

或者一行：

```bash
wl-android-compositor & sleep 1 && KWIN_WAYLAND_BACKEND=1 WAYLAND_DISPLAY=wl-android-0 startplasma-wayland
```

## 原理

```
KDE 应用 → KWin 合成 → wl_surface.commit
                          ↓ wlr_buffer_get_dmabuf()
wl-android-compositor 截获
                          ↓ SCM_RIGHTS
socketd → land-app → Android 屏幕
```

KWin 负责所有桌面合成（窗口管理、特效、动画），渲染到自己的 wl_surface 上。
我们的合成器只截获这个 surface 的 commit，提取 DMA-BUF fd 转发。
KWin 全 GPU 加速，不走 CPU。

## 前提

- KWin 需编译了 `KWIN_BUILD_WAYLAND_BACKEND`（Arch 默认开启）
- 容器内有 GPU 加速（Mesa freedreno + KGSL 透传）

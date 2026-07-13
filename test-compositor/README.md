# wl-android-compositor

无头嵌套 wlroots 合成器。**唯一容器侧组件。**

## 构建

```bash
apt install libwlroots-dev libwayland-dev libdrm-dev
make
```

## 运行

```bash
wl-android-compositor &
WAYLAND_DISPLAY=wl-android-0 weston-simple-egl
```

## 原理

```
Wayland 客户端 → wl-android-compositor → socketd → land-app
                    ↓ wlr_buffer_get_dmabuf()
                    ↓ sendmsg(SCM_RIGHTS)
```

# wl-android-compositor: 无头嵌套 wlroots 合成器

类似 Gamescope 的嵌套合成器，将应用 DMA-BUF 帧通过 land 插件转发到 Android 设备。

## 架构

```
容器内:
  wl-android-compositor (headless wlroots)
    ├── Wayland socket → 应用（Weston 终端 / 游戏 / 任何 wl 应用）
    └── libland_wlroots.so → SCM_RIGHTS → landd

宿主机 (Android):
  landd → land-app (Vulkan 渲染)
```

## 构建

```bash
# 1. 构建 land 插件
cargo build --target x86_64-unknown-linux-gnu --release -p land

# 2. 安装到系统
sudo cp target/x86_64-unknown-linux-gnu/release/libland_wlroots.so \
      /usr/lib/wlroots/

# 3. 构建合成器
cd test-compositor
make
```

依赖: `wlroots >= 0.17`, `wayland-server`, `libdrm`

## 运行

```bash
# 终端 1: 启动合成器
WAYLAND_DISPLAY=wl-android-0 ./test-compositor/wl-android-compositor

# 终端 2: 在该 socket 下运行应用
WAYLAND_DISPLAY=wl-android-0 weston-simple-egl
# 或
WAYLAND_DISPLAY=wl-android-0 glmark2-es2-wayland
```

## 延迟

| 阶段 | 延迟 |
|------|------|
| surface commit → land 插件 | < 500µs |
| DMA-BUF fd 提取 | < 100µs |
| SCM_RIGHTS 发送 | ~100µs |
| landd 转发 | < 50µs |
| Vulkan 导入 + 渲染 | < 1ms (4K) |
| **端到端 (零拷贝)** | **< 3ms** |

## 与 niri 共存

```bash
# niri 在 wayland-0 上运行
# wl-android-compositor 在 wl-android-0 上运行
# 二者独立

# 在 niri 终端中启动应用并指向嵌套合成器
env WAYLAND_DISPLAY=wl-android-0 your-app
```

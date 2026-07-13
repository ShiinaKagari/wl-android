# 容器侧部署指南

## 前置条件

Droidspaces 需将宿主机的 socket 目录 bind mount 进容器：

```yaml
# droidspaces.yml
mounts:
  - /dev/socket:/dev/socket:shared
```

---

## 唯一方案：嵌套合成器（零修改）

继承合成器不修改任何第三方代码。`wl-android-compositor` 启动后作为 Wayland 服务端，
应用连接到它的 socket 即可。

```bash
# 容器内
# Arch
pacman -S wlroots wayland-server libdrm
# Debian/Ubuntu
apt install libwlroots-dev libwayland-dev libdrm-dev

# 1. 构建 land 插件
cargo build --target x86_64-unknown-linux-gnu --release -p land
sudo cp target/release/libland_wlroots.so /usr/lib/wlroots/

# 2. 构建嵌套合成器
cd test-compositor && make
sudo cp wl-android-compositor /usr/local/bin/

# 3. 运行
export LAND_SOCKET=/dev/socket/land.sock
wl-android-compositor &
WAYLAND_DISPLAY=wl-android-0 weston-simple-egl
```

支持的应用：

```bash
WAYLAND_DISPLAY=wl-android-0 weston-simple-egl     # 测试 EGL
WAYLAND_DISPLAY=wl-android-0 glmark2-wayland        # GL 基准 (注意: glmark2-es2 需要 GPU)
WAYLAND_DISPLAY=wl-android-0 gnome-shell --nested & # GNOME 桌面
WAYLAND_DISPLAY=wl-android-0 kwin_wayland --xwayland startplasma-wayland &  # KDE Plasma
WAYLAND_DISPLAY=wl-android-0 your-app               # 任意 Wayland 应用
```

---

## 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `LAND_SOCKET` | `/dev/socket/land.sock` | landd 监听的 socket 路径 |
| `WAYLAND_DISPLAY` | `wayland-0` | 嵌套合成器的显示 socket |

---

## 验证

```bash
# 检查 socket 连通性
ls -la /dev/socket/land.sock
# srw-rw-rw- 1 root root 0 ... land.sock

# 查看合成器日志 (合成器启动后自动打印)
wl-android-compositor
# [compositor] WAYLAND_DISPLAY=wl-android-0
# [land] plugin loaded
# [land] backend ready
```

---

## 一键启动脚本

```bash
#!/usr/bin/env bash
# 容器侧一键启动
set -euo pipefail

export LAND_SOCKET="${LAND_SOCKET:-/dev/socket/land.sock}"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wl-android-0}"

# 等待 landd socket 就绪
for i in $(seq 1 10); do
    [ -S "$LAND_SOCKET" ] && break
    sleep 0.5
done

if [ ! -S "$LAND_SOCKET" ]; then
    echo "landd not reachable at $LAND_SOCKET"
    exit 1
fi

# 启动嵌套合成器
wl-android-compositor &

# 启动应用 (可选, 不加则仅提供 Wayland socket)
if [ $# -gt 0 ]; then
    WAYLAND_DISPLAY=wl-android-0 "$@"
fi
```

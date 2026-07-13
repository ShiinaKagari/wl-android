# 在 wl-android 上启动 GNOME 45 桌面

## 架构

```
Android 宿主机                         Linux 容器 (Droidspaces)
land-app ← SCM_RIGHTS ← landd ← SCM_RIGHTS ← libland_wlroots.so
                                               ↑
                                         wl-android-compositor
                                           (headless wlroots)
                                               ↑ Wayland 客户端
                                         gnome-shell --nested
```

## 容器内依赖

```bash
# Debian/Ubuntu
apt install gnome-session gnome-shell mutter \
            wayland-protocols wlroots libdrm mesa-utils \
            dbus-x11 xdg-dbus-proxy
```

## 启动步骤

### 1. 构建 wl-android 组件

```bash
# 在容器内
cargo build --target x86_64-unknown-linux-gnu --release -p land -p landd
sudo cp target/release/libland_wlroots.so /usr/lib/wlroots/

# 构建嵌套合成器
cd test-compositor && make
sudo cp wl-android-compositor /usr/local/bin/
```

### 2. 启动脚本

创建 `/usr/local/bin/wl-android-gnome45`:

```bash
#!/usr/bin/env bash
set -euo pipefail

# 环境
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export LAND_SOCKET="${LAND_SOCKET:-/dev/socket/land.sock}"

# 1. 启动 landd（如果宿主机没有，则在容器内启动测试用 landd）
if [ ! -S "$LAND_SOCKET" ]; then
    landd &
    sleep 0.5
fi

# 2. 启动嵌套合成器（独占 wl-android-0 socket）
killall wl-android-compositor 2>/dev/null || true
export WAYLAND_DISPLAY=wl-android-0
wl-android-compositor &
sleep 0.5

# 3. 启动 D-Bus session（GNOME 必需）
export DBUS_SESSION_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR}/bus"
if [ ! -S "$DBUS_SESSION_BUS_ADDRESS" ]; then
    dbus-daemon --session --address="$DBUS_SESSION_BUS_ADDRESS" --fork
fi

# 4. 启动 GNOME Shell (nested mode)
export DISPLAY=:0
gnome-shell --nested --wayland \
    --display="$WAYLAND_DISPLAY" \
    2>&1 | tee /tmp/gnome-shell.log
```

### 3. 运行

```bash
chmod +x /usr/local/bin/wl-android-gnome45
wl-android-gnome45
```

## 验证

| 检查项 | 命令 |
|--------|------|
| compositor socket | `ls -la /tmp/.wl-android-0*` |
| 合成器日志 | `tail -f /tmp/gnome-shell.log` |
| landd socket | `ls -la $LAND_SOCKET` |
| 帧是否转发 | `journalctl -f -t landd` |

## 已知限制

| 问题 | 原因 | 缓解 |
|------|------|------|
| GNOME Shell 不支持硬件加速 | 嵌套模式用 llvmpipe 软件渲染 | 在容器内直通 GPU (Droidspaces 支持) |
| 触摸输入未映射 | GNOME Shell 不接收 wl-android-compositor 的 wl_seat 事件 | 需在 compositor 中实现 input forwarding |
| systemd/logind 缺失 | 容器内通常无 systemd | 用 `dbus-run-session` 替代 |

## 可选: 不使用嵌套（Portal 方案）

如果嵌套模式不稳定，改用 land-portal（延迟更高但无需 wlroots）:

```bash
# 容器内需安装 pipewire
apt install pipewire pipewire-pulse wireplumber

# 启动 GNOME 45 普通(非嵌套) session
export XDG_SESSION_TYPE=wayland
gnome-session &

# 启动 land-portal
cargo run --release -p land-portal
```

## 参考

- [Mutter nested mode](https://wiki.gnome.org/Projects/Mutter/Nested)
- [GNOME 45 release notes](https://release.gnome.org/45/)
- [Droidspaces device passthrough](https://droidspaces.dev/docs/gpu)

#!/usr/bin/env bash
# wl-android GNOME 45 启动脚本
# 启动嵌套合成器 → 嵌套 GNOME Shell → DMA-BUF 转发到 Android
set -euo pipefail

: "${LAND_SOCKET:=/dev/socket/land.sock}"
: "${COMPOSITOR_SOCKET:=wl-android-0}"
: "${XDG_RUNTIME_DIR:=/run/user/$(id -u)}"

cleanup() {
    echo "[wl-android] cleaning up..."
    kill %1 %2 %3 2>/dev/null || true
    wait
}
trap cleanup EXIT INT TERM

# 1. landd
if [ ! -S "$LAND_SOCKET" ]; then
    echo "[wl-android] starting landd..."
    landd &
    sleep 1
fi

# 2. wl-android-compositor
echo "[wl-android] starting compositor on $COMPOSITOR_SOCKET..."
export WAYLAND_DISPLAY="$COMPOSITOR_SOCKET"
wl-android-compositor &
sleep 1

# 3. D-Bus session (for GNOME)
export DBUS_SESSION_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR}/bus"
if [ ! -S "$DBUS_SESSION_BUS_ADDRESS" ]; then
    echo "[wl-android] starting dbus session..."
    dbus-daemon --session --address="$DBUS_SESSION_BUS_ADDRESS" --fork
fi

# 4. GNOME Shell nested
echo "[wl-android] starting gnome-shell --nested..."
exec gnome-shell --nested --wayland 2>&1 | tee /tmp/gnome-shell.log

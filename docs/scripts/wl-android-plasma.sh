#!/usr/bin/env bash
# wl-android KDE Plasma 启动脚本
# 嵌套 KWin (KWaylandBackend) + Plasma 桌面 → DMA-BUF 转发到 Android
set -euo pipefail

: "${LAND_SOCKET:=/dev/socket/land.sock}"
: "${WAYLAND_DISPLAY:=wl-android-0}"
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

# 2. wl-android-compositor (headless wlroots)
echo "[wl-android] starting compositor on $WAYLAND_DISPLAY..."
wl-android-compositor &
sleep 1

# 3. D-Bus session (KDE 需要)
export DBUS_SESSION_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR}/bus"
if [ ! -S "$DBUS_SESSION_BUS_ADDRESS" ]; then
    echo "[wl-android] starting dbus session..."
    dbus-daemon --session --address="$DBUS_SESSION_BUS_ADDRESS" --fork
fi

# 4. KWin (KWaylandBackend) + KDE Plasma
echo "[wl-android] starting KWin + Plasma..."
export KWIN_WAYLAND_BACKEND=1
exec kwin_wayland --xwayland --exit-with-session \
    startplasma-wayland 2>&1 | tee /tmp/plasma.log

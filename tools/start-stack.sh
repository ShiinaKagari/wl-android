#!/usr/bin/env bash
# Start the Plasma desktop stack inside the droidspaces container (arch).
# Run as root via droidspaces; the server runs as kagari with the proper
# env. Standard startup order: wl-android server FIRST (provides land-0),
# then startplasma-wayland as kagari.

set -u

runuser_env() {
    # env vars KWin/Plasma need (per docs/DESIGN.md + manual flow)
    runuser -u kagari -- env \
        XDG_RUNTIME_DIR=/run/user/1000 \
        WAYLAND_DISPLAY=land-0 \
        LAND_SOCKET=/run/wl-android/land.sock \
        VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/freedreno_icd.aarch64.json \
        MESA_LOADER_DRIVER_OVERRIDE=kgsl \
        vblank_mode=3 \
        EGL_PLATFORM=wayland \
        QT_QPA_PLATFORM=wayland \
        KWIN_COMPOSE=O2 \
        "$@"
}

start_server() {
    echo "==> starting wl-android (kagari)"
    rm -f /tmp/wl-android.log
    runuser_env setsid /home/kagari/wl-android/target/release/wl-android run \
        > /tmp/wl-android.log 2>&1 < /dev/null &
    sleep 2
    ps aux | grep "wl-android run" | grep -v grep
    echo "==> server log tail:"
    tail -3 /tmp/wl-android.log
}

start_plasma() {
    echo "==> starting startplasma-wayland (kagari)"
    rm -f /tmp/plasma.log
    runuser_env setsid dbus-run-session startplasma-wayland \
        > /tmp/plasma.log 2>&1 < /dev/null &
    sleep 8
    echo "==> plasma processes:"
    ps aux | grep -E "plasmashell|kwin_wayland" | grep -v grep
    echo "==> plasma log tail:"
    tail -10 /tmp/plasma.log
}

case "${1:-all}" in
    server) start_server ;;
    plasma) start_plasma ;;
    all) start_server; start_plasma ;;
    *) echo "usage: $0 [server|plasma|all]"; exit 1 ;;
esac

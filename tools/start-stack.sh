#!/usr/bin/env bash
# Start the Plasma desktop stack inside the droidspaces container (arch).
# Run as root via droidspaces; the server runs as kagari with the proper
# env. Standard startup order: wl-android server FIRST (provides land-0),
# then startplasma-wayland as kagari.

set -u

runuser_env() {
    # env vars KWin/Plasma need (per docs/DESIGN.md + manual flow).
    # ulimit -n: kagari's default soft limit is 1024 — plasmashell's QML
    # scene graph + D-Bus + Wayland + inotify watchers blow past it at
    # startup and the shell BLOCKS mid-init (no crash, just frozen with
    # "Failed to acquire watch file descriptor Too many open files").
    # Raise to the hard limit (32768) before launching anything.
    # ulimit -c: allow core dumps so plasmashell crashes leave a file for
    # analysis (default 0 = no core, silent death with no trace).
    runuser -u kagari -- sh -c 'ulimit -n 32768; ulimit -c unlimited; exec env XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=land-0 LAND_SOCKET=/run/wl-android/land.sock VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/freedreno_icd.aarch64.json MESA_LOADER_DRIVER_OVERRIDE=kgsl vblank_mode=3 EGL_PLATFORM=wayland QT_QPA_PLATFORM=wayland KWIN_COMPOSE=O2 "$@"' -- "$@"
}

start_server() {
    echo "==> killing any existing wl-android server(s)"
    # Single-instance guarantee: a stale server would keep holding land.sock
    # and strand the App on a server KWin never talks to (black screen while
    # everything looks alive). Kill ALL before starting one.
    # NOTE: match the server binary path exactly — a loose -f pattern also
    # matches the runuser wrapper command line and kills it, which takes the
    # freshly-started server down with it (observed: "Killed runuser").
    pkill -9 -x wl-android 2>/dev/null
    pkill -9 -f "target/release/wl-android run$" 2>/dev/null
    sleep 1
    rm -f /run/wl-android/land.sock

    echo "==> starting wl-android (kagari)"
    # Logs must be writable by kagari (this script runs as root via
    # droidspaces; a root-owned log from a previous run makes the redirect
    # fail silently and the server never starts).
    rm -f /tmp/wl-android.log
    touch /tmp/wl-android.log && chown kagari:kagari /tmp/wl-android.log && chmod 666 /tmp/wl-android.log
    runuser_env setsid /home/kagari/wl-android/target/release/wl-android run \
        > /tmp/wl-android.log 2>&1 < /dev/null &
    sleep 2
    echo "==> running servers (must be 1):"
    ps aux | grep "wl-android run" | grep -v grep | grep -v runuser
    echo "==> server log tail:"
    tail -3 /tmp/wl-android.log
}

start_plasma() {
    echo "==> starting startplasma-wayland (kagari)"
    # CRITICAL: systemd user services (kactivitymanagerd etc.) are launched
    # by systemd, NOT by the plasma session process — they only see the env
    # imported into the user manager. Without WAYLAND_DISPLAY/QT_QPA_PLATFORM
    # there, kactivitymanagerd fails to connect to the compositor and aborts,
    # which makes plasmashell abort with "Aborting shell load: The activity
    # manager daemon (kactivitymanagerd) is not running." Import the session
    # env BEFORE starting plasma so systemd-launched KDE daemons can connect.
    runuser -u kagari -- env \
        XDG_RUNTIME_DIR=/run/user/1000 \
        DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
        WAYLAND_DISPLAY=land-0 \
        QT_QPA_PLATFORM=wayland \
        LAND_SOCKET=/run/wl-android/land.sock \
        systemctl --user import-environment \
            WAYLAND_DISPLAY QT_QPA_PLATFORM LAND_SOCKET XDG_RUNTIME_DIR 2>&1 || true

    rm -f /tmp/plasma.log
    touch /tmp/plasma.log && chown kagari:kagari /tmp/plasma.log && chmod 666 /tmp/plasma.log
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

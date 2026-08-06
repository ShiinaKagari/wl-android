#!/bin/bash
# wl-android 快速部署测试脚本（用 droidspaces run 简化）
# 用法:
#   ./deploy-test.sh server   # 重新编译+部署 server binary，重启会话
#   ./deploy-test.sh app      # 重新编译+部署 App .so，重启 App
#   ./deploy-test.sh verify   # 只验证帧流动（不部署）
#   ./deploy-test.sh status   # 查看进程/连接状态

set -e

DEVICE=$(adb devices | awk 'NR==2{print $1}')
if [ -z "$DEVICE" ]; then
    echo "ERROR: 设备未连接"
    exit 1
fi
echo "device: $DEVICE"

DS="/data/local/Droidspaces/bin/droidspaces --name=arch"
REPO=/home/kagari/wl-android

run() {
    adb shell "su -c '$DS run /bin/bash -c \"export PATH=/usr/sbin:/usr/bin:/sbin:/bin; $1\"'"
}

verify() {
    echo "--- verify frames ---"
    adb logcat -c 2>/dev/null || true
    sleep 2
    echo "[T0]"
    adb logcat -d -s land-native 2>/dev/null | grep -E "Frame received|render:" | tail -3 || true
    sleep 3
    echo "[T3]"
    adb logcat -d -s land-native 2>/dev/null | grep -E "Frame received|render:" | tail -3 || true
    echo "--- processes ---"
    adb shell "ps -A | grep -E 'com.wl.android|kwin|plasma'" 2>/dev/null || true
}

show_status() {
    echo "--- sockets ---"
    adb shell "su -c 'ss -xpn 2>/dev/null | grep -E \"land|wayland\"'" 2>/dev/null || true
    echo "--- processes ---"
    adb shell "ps -A | grep -E 'wl-android|kwin|plasma|com.wl.android'" 2>/dev/null || true
}

start_server() {
    echo "--- rebuild server in container ---"
    run "cd $REPO && cargo build --release -p wl-android > /tmp/b.log 2>&1; tail -2 /tmp/b.log"
    echo "--- deploy ---"
    run "cp $REPO/target/release/wl-android /data/local/tmp/wl-android/wl-android"
    echo "--- restart session ---"
    run "pkill plasmashell; pkill kwin_wayland; pkill wl-android; sleep 1" || true
    sleep 2
    run "XDG_RUNTIME_DIR=/tmp/wl-runtime WAYLAND_DISPLAY=land-0 VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/freedreno_icd.aarch64.json MESA_LOADER_DRIVER_OVERRIDE=kgsl vblank_mode=3 MESA_VK_WSI_PRESENT_MODE=mailbox LAND_SOCKET=/run/wl-android/land.sock $REPO/target/release/wl-android run &" &
    sleep 4
    run "XDG_RUNTIME_DIR=/tmp/wl-runtime WAYLAND_DISPLAY=land-0 VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/freedreno_icd.aarch64.json MESA_LOADER_DRIVER_OVERRIDE=kgsl vblank_mode=3 MESA_VK_WSI_PRESENT_MODE=mailbox EGL_PLATFORM=wayland QT_QPA_PLATFORM=wayland KWIN_COMPOSE=O2 HOME=/home/kagari kwin_wayland --no-lockscreen --socket wayland-0 &" &
    sleep 10
    run "XDG_RUNTIME_DIR=/tmp/wl-runtime WAYLAND_DISPLAY=wayland-0 HOME=/home/kagari KDE_FULL_SESSION=true plasmashell &" &
    sleep 10
    adb shell "am force-stop com.wl.android && am start com.wl.android/.MainActivity"
    verify
}

start_app() {
    echo "--- build app .so on host ---"
    (cd android-app/native && cargo +stable ndk -t arm64-v8a build --release 2>&1 | tail -2)
    APP_LIB=$(adb shell "su -c 'find /data/app -name libland_native.so -path \"*wl.android*\"' | head -1")
    echo "--- deploy to $APP_LIB ---"
    adb push android-app/native/target/aarch64-linux-android/release/libland_native.so /sdcard/
    adb shell "su -c 'cp /sdcard/libland_native.so $APP_LIB'"
    echo "--- restart app ---"
    adb shell "am force-stop com.wl.android && am start com.wl.android/.MainActivity"
    verify
}

case "$1" in
    server) start_server ;;
    app) start_app ;;
    verify) verify ;;
    status) show_status ;;
    *)
        echo "usage: $0 [server|app|verify|status]"
        exit 1
        ;;
esac

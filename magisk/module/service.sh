#!/system/bin/sh
# 开机创建 /dev/socket/ (tmpfs) — land-app 启动后在此创建 land.sock
mkdir -p /dev/socket 2>/dev/null
chmod 0755 /dev/socket
log -t "wl-android" "/dev/socket ready"

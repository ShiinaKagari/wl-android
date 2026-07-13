#!/system/bin/sh
# wl-android-daemon: 开机自启 + 保活
# Magisk service.sh — late_start 阶段执行

MODDIR=${0%/*}
SOCKET_DIR=/dev/socket
SOCKET_PATH="$SOCKET_DIR/land.sock"
LANDD="/system/bin/landd"

log_info() { log -t "wl-android" -p i "$1"; }
log_err() { log -t "wl-android" -p e "$1"; }

# 创建 socket 目录 (tmpfs)
mkdir -p "$SOCKET_DIR" || { log_err "mkdir failed"; exit 1; }
chmod 0755 "$SOCKET_DIR"

# 清理前次 socket
[ -S "$SOCKET_PATH" ] && rm -f "$SOCKET_PATH"

# landd 守护
if [ ! -f "$LANDD" ]; then
    log_err "landd not found at $LANDD"
    exit 1
fi

chmod 0755 "$LANDD"

# 后台启动，crash 后自动重启
nohup sh -c '
    while :; do
        '"$LANDD"'
        sleep 1
    done
' &

# 等待 socket 就绪
for i in $(seq 1 10); do
    [ -S "$SOCKET_PATH" ] && break
    sleep 0.2
done

if [ -S "$SOCKET_PATH" ]; then
    chmod 0666 "$SOCKET_PATH"
    log_info "landd ready on $SOCKET_PATH"
else
    log_err "landd failed to create socket"
fi

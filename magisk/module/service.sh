#!/system/bin/sh
# 启动 socketd — 创建并管理 /dev/socket/land.sock
MODDIR=${0%/*}
SOCKETD="$MODDIR/bin/socketd"
while :; do
    if [ -f "$SOCKETD" ]; then
        chmod 0755 "$SOCKETD"
        "$SOCKETD"
    fi
    sleep 2
done

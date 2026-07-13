#!/system/bin/sh
# 启动 socketd — 管理 /data/local/tmp/land.sock
SOCKETD="/system/bin/socketd"
while :; do
    if [ -f "$SOCKETD" ]; then
        chmod 0755 "$SOCKETD"
        "$SOCKETD"
    fi
    sleep 2
done

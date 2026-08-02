# 清理 /data/local/tmp/wl-android/

## 保留
- 无（全部清空重建）

## 删除
```bash
rm -rf /data/local/tmp/wl-android/*
```

包括：
- 旧 server binary（wl-android，无 memfd 代码）
- 中间源文件副本（state.rs, app_link.rs, main.rs, frame_router.rs）
- 旧 socket 文件（land.sock, wayland-*.lock）
- 旧日志（server.log, 14MB）
- 测试程序（test-cb.c, test-frame-callback）
- 启动脚本（start.sh — 之后用新的）

## 容器侧
/run/wl-android/ bind 已解绑，里面残留文件在容器内直接 rm：
```bash
rm -rf /run/wl-android/*
```

## 之后
仅 bind socket 文件，不 bind 目录。server binary 放 /dev/shm 编译。

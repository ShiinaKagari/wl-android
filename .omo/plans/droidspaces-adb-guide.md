# Droidspaces + ADB 调试方案

## 架构概览

```
┌──────────────────────────────────────────────────┐
│ Android Host (OnePlus Pad 3)                      │
│                                                    │
│  /data/local/tmp/wl-android/                       │
│    ├── land.sock  ←────────── bind mount ──────┐  │
│    ├── server.log                              │  │
│    └── wl-android (binary)                     │  │
│                                                    │  │
│  ┌──────────────────────────────────────────┐    │  │
│  │ com.wl.android (App)                     │    │  │
│  │  → land.sock (Unix socket)               │    │  │
│  │  → libland_native.so (Rust JNI)          │    │  │
│  │  → ANativeWindow (SurfaceView)            │    │  │
│  └──────────────────────────────────────────┘    │  │
│                                                    │  │
│  ┌── Droidspaces Arch (container) ─────────────┐ │  │
│  │  /run/wl-android/                            │ │  │
│  │    ├── land.sock  ←── bind mount ────────────┘ │  │
│  │    ├── land-0 (Wayland socket, compositor)      │  │
│  │    └── start.sh                                 │  │
│  │                                                  │  │
│  │  wl-android compositor (Wayland server)          │  │
│  │  KWin / weston-simple-shm (Wayland client)       │  │
│  │  /tmp/wl-android/ (source + build)               │  │
│  └──────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────┘
```

## 容器操作

### 进入容器

```bash
adb shell
su
/data/local/Droidspaces/bin/droidspaces --name=arch enter
```

### 开发流程

```bash
# 一次性的 chown（root）
chown -R $USER:$USER /tmp/wl-android /run/wl-android

# 日常开发（普通用户）
su -l $USER
cd ~/wl-android       # git clone 到哪都行
git pull origin master
cargo build --release -p wl-android

# 启动 compositor
XDG_RUNTIME_DIR=/run/wl-android \
WAYLAND_DISPLAY=land-0 \
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/freedreno_icd.aarch64.json \
./target/release/wl-android run &

# 启动测试客户端（weston 渲染循环）
XDG_RUNTIME_DIR=/run/wl-android \
WAYLAND_DISPLAY=land-0 \
weston-simple-shm &

# 或启动 KWin 桌面
XDG_RUNTIME_DIR=/run/wl-android \
WAYLAND_DISPLAY=land-0 \
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/freedreno_icd.aarch64.json \
kwin_wayland --no-lockscreen
```

## Host 操作

### App 部署

```bash
# 编译 App native 库
cd android-app/native
cargo +stable ndk -t arm64-v8a build --release

# 推送到设备
adb push target/aarch64-linux-android/release/libland_native.so /sdcard/

# 覆盖已安装 app 的 .so（需要 su）
adb shell
su
cp /sdcard/libland_native.so \
   /data/app/~~<hash1>==/com.wl.android-<hash2>==/lib/arm64/libland_native.so

# 获取安装路径（首次）
find /data/app -name "libland_native.so" -path "*wl.android*"
```

### 日志查看

```bash
# App 日志（过滤 wl-android 相关）
adb logcat -s land-native land_native com.wl.android

# 实时 tail
adb logcat -c && adb logcat land-native:* *:S

# 容器 compositor 日志
adb shell cat /data/local/tmp/wl-android/server.log

# 监控关键事件
adb logcat | grep -E "FRAME|handshake|HELO|CONF|surface commit|dispatched"
```

### 进程检查

```bash
# 查看 compositor 和客户端
adb shell ps -A | grep -E "wl-android|kwin|weston"

# 查看 App 进程
adb shell ps -A | grep com.wl.android
```

### Socket 检查

```bash
# Wayland socket（容器内）
adb shell su -c "ls -la /data/local/tmp/wl-android/land-0"

# 看看谁在连
adb shell ss -lnx | grep land
```

## 调试技巧

### WAYLAND_DEBUG

```bash
# 在容器内运行时开启 Wayland 协议调试
WAYLAND_DEBUG=1 kwin_wayland --no-lockscreen 2>&1 | tee /tmp/kwin-debug.log
```

### 容器内安装 strace

```bash
pacman -S strace

# 追踪 KWin 的系统调用
strace -f -e trace=connect,sendmsg,recvmsg,openat \
  kwin_wayland --no-lockscreen 2>&1 | tee /tmp/kwin-strace.log
```

### 分离前后台

```bash
# 容器内后台运行 + 日志重定向
nohup ./wl-android run > /tmp/wl-android.log 2>&1 &
echo $! > /tmp/wl-android.pid

# 停止
kill $(cat /tmp/wl-android.pid)
```

## 常见问题

| 症状 | 原因 | 排查 |
|------|------|------|
| App 黑屏 | 像素数据未到达 | `adb logcat | grep FRAME` 看有没有 `data=XXXB` |
| `Connection refused` | compositor 未启动 | `ss -lnx | grep land` 看 socket 是否存在 |
| `Permission denied` on socket | chown 没做或权限不对 | `ls -la /run/wl-android/land-0` |
| KWin 5帧后停止 | GPU 后端失败 | 检查 `VK_ICD_FILENAMES`，看 KWin stderr |
| `fd count mismatch` | SCM_RIGHTS 传输失败 | `logcat | grep recv_raw` 看收到几个 fd |
| `Text file busy` | 二进制正在运行 | 先 kill 进程再 cp |

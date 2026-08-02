# wl-android 部署测试操作手册

> 目标：从"知道改了什么"到"确认验证结果"控制在 2 分钟内。
> 原则：只编译改动的、只部署改动的、只重启受影响的、只验证关键的。

## 0. 前置事实（固定，不需要每次探查）

```bash
# 设备
DEVICE=376627b8

# 容器访问方式（chroot 到容器根文件系统）
# 先找一个容器内进程 PID，然后：
NS="nsenter -t <PID> -m -p -u -- chroot /proc/<PID>/root /bin/sh -c"

# 容器内路径
CONTAINER_REPO=/home/kagari/wl-android
SERVER_BIN=$CONTAINER_REPO/target/release/wl-android

# host 侧共享路径（bind mount）
SHARED=/data/local/tmp/wl-android

# 运行环境变量（server 启动用）
SERVER_ENV="XDG_RUNTIME_DIR=/tmp/wl-runtime WAYLAND_DISPLAY=land-0 \
  VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/freedreno_icd.aarch64.json \
  LAND_SOCKET=/run/wl-android/land.sock"
```

## 1. 判定改动范围（1 秒）

```bash
git status --short
# 出现：
#   crates/wl-android/src/*.rs  → server 二进制（需容器内编译）
#   android-app/native/src/*    → App .so（host 交叉编译）
#   android-app/app/src/*.kt    → APK（需 gradle 重打包，最慢）
```

## 2. 各组件构建+部署（只做改动的）

### 2a. Server 二进制

```bash
# 容器内编译（唯一需要进容器的场景）
$NS "cd $CONTAINER_REPO && cargo build --release -p wl-android > /tmp/b.log 2>&1"
$NS "cp $CONTAINER_REPO/target/release/wl-android /data/local/tmp/wl-android/wl-android"
```

### 2b. App .so（host 交叉编译，不进容器）

```bash
cd android-app/native
cargo +stable ndk -t arm64-v8a build --release
APP_LIB=$(adb shell "su -c 'find /data/app -name libland_native.so -path \"*wl.android*\"' | head -1")
adb push target/aarch64-linux-android/release/libland_native.so /sdcard/
adb shell "su -c 'cp /sdcard/libland_native.so $APP_LIB'"
```

### 2c. APK（Kotlin 改动，最慢）

```bash
cd android-app && ./gradlew assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

## 3. 重启策略（只重启受影响的）

| 改动 | 需要重启 |
|------|---------|
| server binary | wl-android + KWin + plasmashell（KWin 依赖 server 连接） |
| App .so | 仅 com.wl.android |
| APK | 仅 com.wl.android |

### 3a. 重启 server 会话（server 改动时）

```bash
$NS "pkill plasmashell; pkill kwin_wayland; pkill wl-android; sleep 1"
$NS "$SERVER_ENV $SERVER_BIN run &"     # sleep 4
$NS "XDG_RUNTIME_DIR=/tmp/wl-runtime WAYLAND_DISPLAY=land-0 VK_ICD_FILENAMES=... HOME=/home/kagari kwin_wayland --no-lockscreen --socket wayland-0 &"  # sleep 10
$NS "XDG_RUNTIME_DIR=/tmp/wl-runtime WAYLAND_DISPLAY=wayland-0 HOME=/home/kagari KDE_FULL_SESSION=true plasmashell &"  # sleep 10
adb shell "am force-stop com.wl.android && am start com.wl.android/.MainActivity"
```

### 3b. 只重启 App（App .so / APK 改动时）

```bash
adb shell "am force-stop com.wl.android && am start com.wl.android/.MainActivity"
sleep 5
```

## 4. 验证（1 秒，不 dump 全量）

### 4a. 帧流动（核心信号）

```bash
# 只抓 land-native tag，只找关键行
adb logcat -d -s land-native | grep -E "Frame received|render:" | tail -3
# 等 3 秒再抓，对比 serial 是否递增
sleep 3
adb logcat -d -s land-native | grep -E "Frame received|render:" | tail -3
```

**通过标准**：两次数值递增（serial 变大），出现 `render: buf 3392x2400`。

### 4b. App 状态

```bash
adb shell "ps -A | grep com.wl.android"   # 进程在 = 未闪退
```

### 4c. Server 产帧

```bash
$NS "grep -c 'sending frame' /tmp/wl-runtime/wl-android.log 2>/dev/null"
# 数值应在增长
```

## 5. 已知环境事实（避免重复踩坑）

| 现象 | 原因 | 处理 |
|------|------|------|
| screencap 黑屏 | wl-android 用硬件 overlay 绕过 SurfaceFlinger | **不要用 screencap 验证**，看 App 的 `render:` 日志 |
| KWin 用 dmabuf 不产 SHM 帧 | 需要禁 dmabuf 全局 | 已在 state.rs 禁用 |
| KWin ZINK 报错 | turnip 不支持 ZINK | 非致命，KWin 仍能产初始帧 |
| `fd count mismatch` | frame_cache fd 序列化 bug | 待修，见 issue |
| git push 超时 | 设备网络不稳定 | 本地 commit，网络恢复后 push |
| 容器内 cargo 编译 | host 缺 aarch64 库 | 必须进容器编译 server |

## 6. 慢操作清单（尽量少用）

| 操作 | 时间 | 替代 |
|------|------|------|
| 容器内全量 `cargo build` | 40s-3min | 只重编译改动的 crate |
| `adb install` APK | 30s-1min | 仅 Kotlin 改动才需要 |
| 全量 logcat dump | 10s+ | `-s land-native` 定向抓 |
| 截图分析 | 1-3min | 只在需要视觉确认时用，用 python 快速分析 |
| subagent 委托 | 1-15min | 常规部署测试直接执行，不委托 |

## 7. 测试标准流程（速查）

```bash
# 改了什么？
git status --short

# ① server 改动 →
#    容器内编译 → cp → 重启会话 → 验证
# ② App .so 改动 →
#    host ndk 编译 → push → 重启 App → 验证
# ③ Kotlin 改动 →
#    gradle → install → 重启 App → 验证

# 验证命令（固定）：
adb logcat -d -s land-native | grep -E "Frame received|render:" | tail -3
sleep 3
adb logcat -d -s land-native | grep -E "Frame received|render:" | tail -3
# serial 递增 = PASS
```

# wl-android 部署测试操作手册

> 目标：从"知道改了什么"到"确认验证结果"控制在 2 分钟内。
> 原则：只编译改动的、只部署改动的、只重启受影响的、只验证关键的。

## 0. 前置事实（固定，不需要每次探查）

```bash
# 设备
DEVICE=376627b8

# 容器访问方式（与 scripts/deploy-test.sh 一致：droidspaces run 一次一命令）
NS() { adb shell "su -c '/data/local/Droidspaces/bin/droidspaces --name=arch run /bin/bash -c \"export PATH=/usr/sbin:/usr/bin:/sbin:/bin; $1\"'"; }
# 交互式长会话（排查时）：adb shell → su → /data/local/Droidspaces/bin/droidspaces --name=arch enter kagari

# 容器内路径
CONTAINER_REPO=~/wl-android

# host 侧共享路径（bind mount）
SHARED=/data/local/tmp/wl-android:/run/wl-android

# 运行环境变量（server 与 KWin 共用，KGSL 必需；缺少会导致 KWin 走 SHM 回退）
KGSL_ENV="XDG_RUNTIME_DIR=/tmp/wl-runtime WAYLAND_DISPLAY=land-0 \
  VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/freedreno_icd.aarch64.json \
  MESA_LOADER_DRIVER_OVERRIDE=kgsl vblank_mode=3 MESA_VK_WSI_PRESENT_MODE=mailbox \
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
NS "cd $CONTAINER_REPO && cargo build --release -p wl-android > /tmp/b.log 2>&1; tail -2 /tmp/b.log"
NS "cp $CONTAINER_REPO/target/release/wl-android /data/local/tmp/wl-android/wl-android"
```

### 2c. APK（Kotlin 改动，最慢）

```bash
cd android-app && ./gradlew assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

## 3. 重启 server 会话（server 改动时）

> **真机验证过的最短序列，顺序有讲究**。`startplasma-wayland` 是 ELF 启动器
> （拉起 plasma_session / kwin_wayland / kded6 / plasmashell），**必须先有
> dbus session**，否则 plasmashell 崩。不要手动拆成 `kwin_wayland` + `plasmashell`
> 两条命令。

```bash
# 1. 清场
NS "pkill startplasma-wayland; pkill wl-android; sleep 1" || true

# 2. XDG_RUNTIME_DIR 必须 700（dbus-daemon 拒绝 0777；adb shell 默认 0777）
NS "chmod 700 /tmp/wl-runtime"

# 3. 先起 dbus session（startplasma-wayland 依赖；必须先于 wl-android / Plasma）
NS "dbus-daemon --session --address=unix:path=/tmp/wl-runtime/bus --fork"

# 4. 起 server（KGSL env；输出重定向供 4c 查帧）
NS "$KGSL_ENV $CONTAINER_REPO/target/release/wl-android run >> /tmp/wl-runtime/wl-android.log 2>&1 &"   # sleep 2

# 5. 一条命令拉起整个 Plasma 会话
#    qdbus 依赖 /usr/bin/qdbus 符号链接（见 §5）；DBUS_SESSION_BUS_ADDRESS 必须显式给，
#    否则 startplasmacompositor 报 "Could not start D-Bus. Can you call qdbus?"
NS "$KGSL_ENV HOME=/home/kagari DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/wl-runtime/bus startplasma-wayland &"   # sleep 5

# 6. 重启 App
adb shell "am force-stop com.wl.android && am start com.wl.android/.MainActivity"
```

只重启 App（App .so / APK 改动时）：

```bash
adb shell "am force-stop com.wl.android && am start com.wl.android/.MainActivity"
sleep 5
```

## 4. 验证（1 秒，不 dump 全量）

### 4a. 帧流动（核心信号）

```bash
# 只抓 land-native tag，只找关键行（swapchain 主路径日志是 present:）
adb logcat -d -s land-native | grep -E "Frame received|render:|present:" | tail -3
# 等 3 秒再抓，对比 serial 是否递增
sleep 3
adb logcat -d -s land-native | grep -E "Frame received|render:|present:" | tail -3
```

**通过标准**：两次数值递增（serial 变大），出现 `present: slot=N`（无 CPU 像素拷贝）。

### 4b. App 状态

```bash
adb shell "ps -A | grep com.wl.android"   # 进程在 = 未闪退
```

### 4c. Server 产帧

```bash
NS "grep -c 'blit frame' /tmp/wl-runtime/wl-android.log 2>/dev/null"
# 数值应在增长（server 启动需带 §3 的重定向）
```

## 5. 已知环境事实（避免重复踩坑）

| 现象 | 原因 | 处理 |
|------|------|------|
| plasma.log 出现 "startplasmacompositor: Could not start D-Bus. Can you call qdbus?" | startplasma-wayland 需要 `qdbus`：容器必须存在 `/usr/bin/qdbus -> /usr/lib/qt6/bin/qdbus` 符号链接（Arch 系仅提供 qdbus6）；缺失时 KWin/Plasma 不启动 → 黑屏 | 修复：容器内 `ln -s /usr/lib/qt6/bin/qdbus /usr/bin/qdbus`（容器重建后需重新执行） |
| plasmashell 直接崩（单独 `plasmashell &`） | startplasma-wayland 依赖已存在的 dbus session；缺 dbus 时 plasmashell 无 session bus 崩 | 先 `dbus-daemon --session --address=unix:path=$XDG_RUNTIME_DIR/bus --fork` 再 `startplasma-wayland`（§3 顺序） |
| dbus-daemon 拒绝启动 / 权限错 | $XDG_RUNTIME_DIR 权限 0777（adb shell 默认） | `chmod 700 $XDG_RUNTIME_DIR` |
| 背靠背 TBUF+native_handle 断连（旧 bug） | SOCK_STREAM 下两条消息合并进一次 recvmsg，旧实现丢尾部字节+fd | **已修复**：transport 的 pending 读前瞻缓冲保留合并字节与 fd（P-18/P-19），有回归测试，不再复现 |
| 日志 "SHM frame dropped" | dmabuf 全局**已启用**（state.rs，blit 主路径要求 dmabuf）；KWin 侧 dmabuf 回退到 SHM 时帧被丢弃 | 确认 server/KWin 都带 KGSL env（`VK_ICD_FILENAMES` / `MESA_LOADER_DRIVER_OVERRIDE=kgsl`）；仅 `LAND_MODE=shm` 保留 SHM 调试路径 |
| screencap 黑屏 | wl-android 用硬件 overlay 绕过 SurfaceFlinger | **不要用 screencap 验证**，看 App 的 `present: slot=N` 日志 |
| KWin ZINK 报错 | turnip 不支持 ZINK | 非致命，KWin 仍能产帧（dmabuf 路径） |
| git push 超时 | 设备网络不稳定 | 本地 commit，网络恢复后 push |
| 容器内 cargo 编译 | host 缺 aarch64 库 | 必须进容器编译 server（§2a） |

## 6. 测试标准流程（速查）

```bash
# 改了什么？
git status --short

# ① server 改动 →
#    容器内编译 → cp → §3 重启会话 → 验证
# ② App .so 改动 →
#    host ndk 编译 → push → 重启 App → 验证
# ③ Kotlin 改动 →
#    gradle → install → 重启 App → 验证

# 验证命令（固定）：
adb logcat -d -s land-native | grep -E "Frame received|render:|present:" | tail -3
sleep 3
adb logcat -d -s land-native | grep -E "Frame received|render:|present:" | tail -3
# serial 递增 = PASS
```

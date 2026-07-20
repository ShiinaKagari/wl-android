# Device Test Plan — wl-android M2-M6

> 在 OnePlus Pad 3 (Snapdragon 8 Elite / Adreno 830) 上执行真机验证。
> 前置条件完成后方可开始测试。

## 前置条件检查清单

### A. 宿主 (Android)
- [ ] Magisk 模块已刷入（`/data/local/tmp/wl-android/` 已创建, 权限 0777）
- [ ] Droidspaces 已配置 bind mount：`/data/local/tmp/wl-android/` ↔ 容器 `/run/wl-android/`
- [ ] 验证 bind mount：容器内 `touch /run/wl-android/test` → 宿主 `adb shell ls /data/local/tmp/wl-android/test`

### B. 容器 (Droidspaces)
- [ ] Mesa 已安装，含 a830 KGSL turnip 支持（`vulkaninfo` 显示 "Turnip" 或 Adreno GPU）
- [ ] `libwayland-server` 已安装
- [ ] `weston-info` 已安装（`apt install weston`）
- [ ] `/dev/kgsl-3d0` 存在于容器内
- [ ] `XDG_RUNTIME_DIR` 已设置

### C. 编译
- [ ] `wl-android` aarch64 二进制已编译（交叉编译或容器内编译）
- [ ] `scripts/mock-app.py` 已推送：`adb push scripts/mock-app.py /data/local/tmp/`

---

## 步骤 1：M2 — Wayland 服务端骨架

```bash
# 容器内
cd /run/wl-android
WAYLAND_DISPLAY=land-0 XDG_RUNTIME_DIR=/tmp ./wl-android run &
sleep 1

# 检查 socket
ls -la /tmp/land-0
# 预期: srwxr-xr-x ... /tmp/land-0

# 协议对象枚举
WAYLAND_DISPLAY=land-0 weston-info 2>&1 | grep -E "wl_compositor|xdg_wm_base|wl_shm|wl_seat|wl_output"
# 预期: 每个协议至少一行

# doctor
./wl-android doctor
```

| 检查 | 预期 | 实测 |
|---|---|---|
| `/tmp/land-0` 存在 | ✅ | |
| compositor protocol | ✅ present | |
| xdg_wm_base protocol | ✅ present | |
| wl_shm protocol | ✅ present | |
| wl_seat protocol | ✅ present | |
| wl_output protocol | ✅ present | |
| doctor 无 ❌ | ✅ all OK | |

---

## 步骤 2：M3 — App 连接 + 握手 + 帧循环

```bash
# 容器内 (保持 wl-android 运行)
LAND_LOG=info WAYLAND_DISPLAY=land-0 XDG_RUNTIME_DIR=/tmp \
  LAND_SOCKET=/run/wl-android/land.sock ./wl-android run &

# 宿主侧 (adb shell)
python3 /data/local/tmp/mock-app.py /data/local/tmp/wl-android/land.sock
```

| 检查 | 预期 | 实测 |
|---|---|---|
| App connect 成功 | ✅ | |
| HELO magic OK | ✅ 0x4F4C4548 | |
| CONF 发送 | ✅ no error | |
| 日志 "handshake complete" | ✅ | |
| 无 crash | ✅ | |

---

## 步骤 3：M4 — 触摸注入

```bash
# 运行 mock-app.py 的触摸部分（见脚本 M4 段）
LAND_LOG=debug WAYLAND_DISPLAY=land-0 LAND_SOCKET=/run/wl-android/land.sock \
  ./wl-android run 2>&1 | grep -i touch
```

| 检查 | 预期 | 实测 |
|---|---|---|
| "touch down" 日志 | ✅ | |
| "touch up" 日志 | ✅ | |
| "touch frame" (no crash) | ✅ | |

---

## 步骤 4：M5 — 动态配置

```bash
# mock-app.py 发送 Config 更新（见脚本 M5 段）
# 容器内验证
WAYLAND_DISPLAY=land-0 weston-info 2>&1 | grep -A2 mode:
```

| 检查 | 预期 | 实测 |
|---|---|---|
| 初始 mode = 3392×2400 @144Hz | ✅ | |
| 更新后 mode = 1920×1080 @60Hz | ✅ | |
| 恢复后 mode = 3392×2400 @144Hz | ✅ | |
| 日志 "applying config update" x2 | ✅ | |

---

## 步骤 5：M6 — KWin/Plasma 连接

```bash
# 容器内
WAYLAND_DISPLAY=land-0 kwin_wayland &
sleep 2

# 检查 KWin 连接状态
WAYLAND_DISPLAY=land-0 weston-info 2>&1 | head -20
# 预期: 看到 xdg_toplevel 等 KWin 创建的 surface

# 或者用 WAYLAND_DEBUG 启动 KWin
WAYLAND_DEBUG=1 WAYLAND_DISPLAY=land-0 kwin_wayland 2>&1 | head -30
# 预期: 无 protocol error
```

| 检查 | 预期 | 实测 |
|---|---|---|
| KWin 连接无 protocol error | ✅ | |
| KWin 创建 xdg_toplevel | ✅ | |
| 日志 "new_toplevel" | ✅ | |
| 无 crash | ✅ | |

---

## 步骤 6：M7 — 性能浸泡

```bash
# 容器内，启动 wl-android + KWin 后
bash scripts/soak.sh 3600 soak-results/
```

| 检查 | 约束 | 实测 |
|---|---|---|
| RSS < 32MB (PERF-05) | ✅ final < 32000KB | |
| fd leak = 0 (PERF-07) | ✅ initial == final | |
| 1h 无 crash | ✅ uptime OK | |

---

## 完成标准

- [ ] M2-M6 全部检查项 ✅
- [ ] 全过程无 SIGSEGV / panic / fd 耗尽
- [ ] PERF-05/07 达标
- [ ] 可与 KWin/Plasma 正常交互

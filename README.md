# wl-android

在 Android 设备上显示 Droidspaces 容器内的完整 Linux 桌面（KDE Plasma 等），
GPU 硬件加速、零 CPU 像素拷贝、触摸交互、旋转/刷新率动态适配。

```bash
# 容器内 (Droidspaces) — 顺序有讲究：dbus 必须先于 Plasma（见 docs/TESTING.md §3）
chmod 700 $XDG_RUNTIME_DIR
dbus-daemon --session --address=unix:path=$XDG_RUNTIME_DIR/bus --fork
wl-android &            # 需带 KGSL 环境变量（见下方"环境变量"）
export WAYLAND_DISPLAY=land-0
startplasma-wayland &

# 安卓端：打开 wl-android-app → 桌面出现在屏幕上
```

## 项目定位

wl-android 是一个**透明中间层**：对合成器（KWin/Weston/Hyprland）表现为一个标准
Wayland 合成器；对 Android 表现为一个普通 App。零 hook、不修改任何第三方代码，
全部行为建立在公开契约上。详见 [BOUNDARIES.md](BOUNDARIES.md)。

## 架构

```
KWin ──标准 Wayland 协议(land-0)──▶ wl-android (容器内, Rust/Smithay)
                                        │  turnip blit + SYNC_FD 栅栏导出
                                        │  land.sock: 二进制协议 + SCM_RIGHTS(AHB slot)
                                        ▼
                              wl-android-app (Kotlin + Rust JNI)
                                        │  Vulkan swapchain 呈现（BRDY 拉式，v2 主路径）
                                        │  MotionEvent → 触摸回注
                                        ▼
                              Magisk 模块（仅目录 + sepolicy）
```

帧路径（v2 主路径：blit + Vulkan swapchain，见 docs/DESIGN.md §5/§6.3）：

- **blit（主路径）**：App 预注册 AHardwareBuffer slot 池（即 swapchain 图像），容器侧
  turnip 将 KWin 帧 blit 进 slot，经 `VK_KHR_external_fence` SYNC_FD 栅栏同步后由 App
  Vulkan swapchain 呈现；BRDY 拉式节奏，零 CPU 像素拷贝。
- **direct**：KWin 的 dmabuf fd 直达 App 导入。ADR #15 确认 830 宿主驱动不支持
  `VK_EXT_external_memory_dma_buf`，该路径在目标设备上不可用。
- **SHM/CPU**：frame_cache memfd 像素拷贝路径，已退役，仅 `LAND_MODE=shm` 调试保留。

## 文档索引

| 文档 | 内容 |
|------|------|
| [docs/DESIGN.md](docs/DESIGN.md) | 协议字节布局、状态机、时序图、模块 API、可测性架构（**改代码前先读**） |
| [BOUNDARIES.md](BOUNDARIES.md) | 边界约束：零 hook 原则、灰区登记、依赖准入 |
| [PERFORMANCE_BOUNDARIES.md](PERFORMANCE_BOUNDARIES.md) | 性能硬约束与测量方法 |

## 环境要求（用户自行准备）

### 目标设备
一加平板 3（Snapdragon 8 Elite / Adreno 830, 3392×2400 @144Hz）。其他 8 Elite
机型理论兼容，未验证。

### 容器内 (Droidspaces)
- Linux 发行版（Ubuntu/Debian），KDE Plasma（`startplasma-wayland` 可用）
- **Mesa：mesa-for-android-container 提供的含 Adreno 830 KGSL 支持的版本
  （≥ 26.1 系）**，turnip + freedreno/zink；`vulkaninfo` 正常、支持
  `VK_KHR_external_memory_fd` + dma_buf 导出
- `libwayland` 系统库；`XDG_RUNTIME_DIR` 已设置
- Droidspaces 配置：`/dev/kgsl-3d0`、`/dev/dma_heap` 映射进容器；
  宿主 `/data/local/tmp/wl-android/` bind mount 到容器 `/run/wl-android/`

### 宿主机 (Android)
- Android 13+（minSdk 33，targetSdk 36），Magisk 已安装
- 本项目不安装/不排查 Mesa 与驱动（见 BOUNDARIES.md §5）

## 代码结构（规划）

```
wl-android/
├── crates/
│   ├── wl-android-common/   # 协议单一事实源（两端同源编译）+ 测试基建
│   └── wl-android/          # 服务端：Smithay 合成器 + frame_router + doctor
├── android-app/             # Kotlin UI + Rust JNI (cargo-ndk, 无 C++ 层)
├── magisk-module/           # 目录 + sepolicy（无业务逻辑）
├── milestones/               # M2~M7 真机验证脚本
├── m0/                       # M0 探测件（独立 crate）
├── scripts/                  # build-all / container-probe / m0-build / soak
└── docs/DESIGN.md
```

## 开发范式

### 三端拓扑

项目涉及三个环境，术语全局统一：

- **开发机** (dev machine)：运行 agent 的主机，git 仓库所在；**代码改动只允许在此进行**。
- **测试后端** (test backend)：安卓设备容器 Droidspaces `--name=arch`；除 App 测试外的一切（server 构建/运行/日志、Plasma/KWin 会话、doctor、soak）。
- **测试前端** (test frontend)：安卓机本身（设备 376627b8）；仅 App 测试（安装/启动/logcat/交互验证）。

源码流转只走 git：开发机 `git push` → 测试后端 `git pull` 后构建/运行，禁止
tarball/手工拷贝等非 git 源码同步；部署产物（编译出的二进制）例外，可经既有 bind
mount 传递。详见 [docs/AGENTS.md](docs/AGENTS.md)。

- **分层 TDD**：协议/状态机/fd 生命周期严格红绿重构；Wayland 行为用 FakeCompositor
  （wayland-client 无头客户端）测试先行；驱动相关薄壳真机验证后以 doctor 断言固化。
  规则编号（P/H/F/C/T/O/X/PERF-xx）与测试一一对应，见 docs/DESIGN.md。
- **库优先**：Smithay/calloop/zerocopy/ash/jni 等，仅手写粘合逻辑（<30%）。
- 每个里程碑：验收测试先行 → 实现 → mock 回归 → **真机验证**（`milestones/M{x}-verify.sh`）。

## 里程碑

| # | 交付 | 真机验证 (milestones/) | CI 测试 | 状态 |
|---|---|------|------|
| M0 | 宿主 Vulkan 探测 + 容器环境诊断 + socket fd 冒烟 | 手动 adb 运行 probe / 脚本跑 probe-container.sh | — | ✅ 完成 |
| M1 | `wl-android-common`：协议 + golden bytes + proptest + 测试基建 | 无需真机 | `cargo test` 35/35 绿 | ✅ 完成 |
| M2 | Smithay 起 land-0；FakeCompositor 帧到达 | `M2-verify.sh`: weston-info 协议对象枚举、doctor 自检 | mock-app 集成回归 | [ ] |
| M3 | App Vulkan swapchain 上屏 + BRDY 拉式回压 | `M3-verify.sh`: socket 连接、slot 注册、帧循环、视觉正确 | FakeCompositor 帧到达 | [ ] |
| M4 | 多点触控注入 | `M4-verify.sh`: 单点/拖拽/多点/边缘/FRAME sentinel | TouchMessage 单元测试 | [ ] |
| M5 | 旋转 / 144Hz / 分辨率变化动态适配 | `M5-verify.sh`: 旋转→桌面跟随、`wm size`→适配、刷新率切换 | MockClock 节拍验证 | [ ] |
| M6 | KWin/Plasma 拉起；Weston/Hyprland 兼容 | `M6-verify.sh`: Plasma 可见、触摸交互、窗口操作、旋转+Plasma | 协议缺失扫描 | [ ] |
| M7 | 性能收口 + Magisk 打包 | `M7-verify.sh`: 1h soak、PERF-01~15 全达标、doctor report | criterion bench 无回归 | [ ] |

## 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `WAYLAND_DISPLAY` | `land-0` | 服务端绑定 `$XDG_RUNTIME_DIR/land-0`（与系统 wayland-0 隔离） |
| `LAND_SOCKET` | `/run/wl-android/land.sock` | 与 App 通信的 socket（服务端 listen） |
| `LAND_MODE` | `auto` | `auto\|blit\|shm`；`shm` 启用已退役的 SHM/CPU 调试帧路径，其余取值走 blit + swapchain 主路径 |
| `LAND_LOG` | `info` | `error\|info\|debug\|proto` |

> **KGSL 环境（KWin/server 启动必需，deploy-test.sh 内联设置）**：
> `VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/freedreno_icd.aarch64.json`、
> `MESA_LOADER_DRIVER_OVERRIDE=kgsl`、`vblank_mode=3`、
> `MESA_VK_WSI_PRESENT_MODE=mailbox`。缺任一都会导致 KWin 走 SHM 回退、帧被丢弃
> （blit 主路径要求 dmabuf）。

## 排障

`wl-android doctor`（容器）与 App 内调试页：自检 socket 权限、协议版本、caps、
fd 往返、Vulkan 能力，并输出延迟/帧率统计。跨端问题按日志中的 `serial` 对齐定位。

真机踩坑（均为已验证事实）：

- **startplasma-wayland 必须先有 dbus**：`dbus-daemon --session
  --address=unix:path=$XDG_RUNTIME_DIR/bus --fork` 之后再 `startplasma-wayland &`；
  单独 `plasmashell &` 缺 session bus 直接崩。序列见 docs/TESTING.md §3。
- **swapchain 延迟分配（DEFERRED_MEMORY_ALLOCATION）**：未 acquire 过的 swapchain
  图像 `vkGetImageMemoryRequirements` 返回 `memory_type_bits=0`（无后备存储），初始化
  必须先把每个图像 acquire 一次，再查需求并绑定专属 AHB-exportable 内存，最后把全部
  图像 present 回呈现引擎，否则帧循环首次 acquire 永久阻塞。真机修复。
- **SOCK_STREAM 消息合并（已修复）**：App 背靠背发送 TBUF + native_handle 会在一次
  recvmsg 内合并；transport 的 pending 读前瞻缓冲保留合并的尾部字节与 fd
  （FD-ORDERING），旧实现丢 handle 字节导致断连，现已修复并有回归测试（P-18/P-19）。

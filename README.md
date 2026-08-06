# wl-android

在 Android 设备上显示 Droidspaces 容器内的完整 Linux 桌面（KDE Plasma 等），
SHM 帧拷贝 + 触摸交互、旋转/刷新率动态适配。

```bash
# 容器内 (Droidspaces) — 顺序有讲究：dbus 必须先于 Plasma（见 docs/TESTING.md §3）
chmod 700 $XDG_RUNTIME_DIR
dbus-daemon --session --address=unix:path=$XDG_RUNTIME_DIR/bus --fork
wl-android &
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
                                        │  SHM 帧拷贝 → memfd (FrameCache, 单次拷贝)
                                        │  land.sock: 二进制协议 + SCM_RIGHTS(像素 fd)
                                        ▼
                              wl-android-app (Kotlin + Rust JNI)
                                        │  ANativeWindow_lock CPU 呈现
                                        │  MotionEvent → 触摸回注
                                        ▼
                              Magisk 模块（仅目录 + sepolicy）
```

帧路径（SHM-only，见 docs/DESIGN.md §5）：

- **SHM（唯一路径）**：KWin commit 的 SHM buffer 经 `FrameCache::push_from`
  直接拷入常驻 memfd（PERF-12 单次拷贝，无中间 Vec），像素 fd 经 land.sock
  送达 App，App 用 `ANativeWindow_lock` 呈现。
- **blit（已移除）**：设备实测 turnip import App AHB 必 SIGSEGV（device-verified），
  blit/swapchain/slot/fence 整套管线已删除（commit b3491f4，-5498 LOC）。
- **direct**：ADR #15 确认 830 宿主驱动不支持 `VK_EXT_external_memory_dma_buf`，
  不可用。

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
│   └── wl-android/          # 服务端：Smithay 合成器 + FrameCache(SHM) + doctor
├── android-app/             # Kotlin UI + Rust JNI (cargo-ndk, 无 C++ 层)
├── magisk-module/           # 目录 + sepolicy（无业务逻辑）
├── scripts/                  # build-all / deploy-test / soak
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
- **库优先**：Smithay/calloop/zerocopy/jni 等，仅手写粘合逻辑（<30%）。
- 每个里程碑：验收测试先行 → 实现 → mock 回归 → **真机验证**（`milestones/M{x}-verify.sh`）。

## 里程碑

| # | 交付 | 真机验证 | CI 测试 | 状态 |
|---|---|------|------|
| M0 | 宿主 Vulkan 探测 + 容器环境诊断 + socket fd 冒烟 | 已完成（结论入 DESIGN.md ADR #5/#15） | — | ✅ 完成 |
| M1 | `wl-android-common`：协议 + golden bytes + proptest + 测试基建 | 无需真机 | `cargo test` 全绿 | ✅ 完成 |
| M2 | Smithay 起 land-0；FakeCompositor 帧到达 | 按 DESIGN.md §14（V-xx 验收表） | mock-app 集成回归 | [ ] |
| M3 | App CPU 呈现上屏（ANativeWindow_lock + FrameCache 帧） | 按 DESIGN.md §14 | FakeCompositor 帧到达 | [ ] |
| M4 | 多点触控注入 | 按 DESIGN.md §14 | TouchMessage 单元测试 | [ ] |
| M5 | 旋转 / 144Hz / 分辨率变化动态适配 | 按 DESIGN.md §14 | MockClock 节拍验证 | [ ] |
| M6 | KWin/Plasma 拉起；Weston/Hyprland 兼容 | 按 DESIGN.md §14 | 协议缺失扫描 | [ ] |
| M7 | 性能收口 + Magisk 打包 | 按 DESIGN.md §14 + soak.sh | criterion bench 无回归 | [ ] |

> 注：历史验证脚本（milestones/）与 M0 探测件（m0/）已归档移除，
> 验收以 docs/DESIGN.md §14 的 V-xx 列表为权威。

## 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `WAYLAND_DISPLAY` | `land-0` | 服务端绑定 `$XDG_RUNTIME_DIR/land-0`（与系统 wayland-0 隔离） |
| `LAND_SOCKET` | `/run/wl-android/land.sock` | 与 App 通信的 socket（服务端 listen） |
| `LAND_LOG` | `info` | `error\|info\|debug\|proto` |

> **KGSL 环境（KWin 渲染必需，deploy-test.sh 内联设置）**：
> `VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/freedreno_icd.aarch64.json`、
> `MESA_LOADER_DRIVER_OVERRIDE=kgsl`、`vblank_mode=3`。
> 若 KWin 无 GPU 渲染退化为软件合成，产出仍为 SHM buffer（服务端唯一消费路径），
> 只是帧率受 CPU 合成上限约束。

## 排障

`wl-android doctor`（容器）与 App 内调试页：自检 socket 权限、协议版本、caps、
fd 往返，并输出延迟/帧率统计。跨端问题按日志中的 `serial` 对齐定位。

真机踩坑（均为已验证事实）：

- **startplasma-wayland 必须先有 dbus**：`dbus-daemon --session
  --address=unix:path=$XDG_RUNTIME_DIR/bus --fork` 之后再 `startplasma-wayland &`；
  单独 `plasmashell &` 缺 session bus 直接崩。序列见 docs/TESTING.md §3。
- **SOCK_STREAM 消息合并（已修复）**：背靠背发送的消息会在一次 recvmsg 内合并；
  transport 的 pending 读前瞻缓冲保留合并的尾部字节与 fd（FD-ORDERING），旧实现丢
  字节导致断连，现已修复并有回归测试（P-18/P-19）。

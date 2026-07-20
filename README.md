# wl-android

在 Android 设备上显示 Droidspaces 容器内的完整 Linux 桌面（KDE Plasma 等），
GPU 硬件加速、零 CPU 像素拷贝、触摸交互、旋转/刷新率动态适配。

```bash
# 容器内 (Droidspaces)
wl-android &
export WAYLAND_DISPLAY=land-0
startplasma-wayland

# 安卓端：打开 wl-android-app → 桌面出现在屏幕上
```

## 项目定位

wl-android 是一个**透明中间层**：对合成器（KWin/Weston/Hyprland）表现为一个标准
Wayland 合成器；对 Android 表现为一个普通 App。零 hook、不修改任何第三方代码，
全部行为建立在公开契约上。详见 [BOUNDARIES.md](BOUNDARIES.md)。

## 架构

```
KWin ──标准 Wayland 协议(land-0)──▶ wl-android (容器内, Rust/Smithay)
                                        │  land.sock: 二进制协议 + SCM_RIGHTS(dmabuf fd)
                                        ▼
                              wl-android-app (Kotlin + Rust JNI)
                                        │  Vulkan 导入 → SurfaceView 上屏
                                        │  MotionEvent → 触摸回注
                                        ▼
                              Magisk 模块（仅目录 + sepolicy）
```

帧路径双模式（运行时协商，见 docs/DESIGN.md §5）：

- **direct**：KWin 的 dmabuf fd 直达 App，Vulkan `VK_EXT_external_memory_dma_buf` 导入；
- **blit**：App 预注册 AHardwareBuffer 池，容器侧 Vulkan blit（宿主驱动不支持
  dma_buf 导入时的兜底，仍零 CPU 拷贝）。

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
| M3 | App 上屏 + cum-ack 回压 | `M3-verify.sh`: socket 连接、slot 注册、帧循环、视觉正确 | FakeCompositor 帧到达 | [ ] |
| M4 | 多点触控注入 | `M4-verify.sh`: 单点/拖拽/多点/边缘/FRAME sentinel | TouchMessage 单元测试 | [ ] |
| M5 | 旋转 / 144Hz / 分辨率变化动态适配 | `M5-verify.sh`: 旋转→桌面跟随、`wm size`→适配、刷新率切换 | MockClock 节拍验证 | [ ] |
| M6 | KWin/Plasma 拉起；Weston/Hyprland 兼容 | `M6-verify.sh`: Plasma 可见、触摸交互、窗口操作、旋转+Plasma | 协议缺失扫描 | [ ] |
| M7 | 性能收口 + Magisk 打包 | `M7-verify.sh`: 1h soak、PERF-01~15 全达标、doctor report | criterion bench 无回归 | [ ] |

## 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `WAYLAND_DISPLAY` | `land-0` | 服务端绑定 `$XDG_RUNTIME_DIR/land-0`（与系统 wayland-0 隔离） |
| `LAND_SOCKET` | `/run/wl-android/land.sock` | 与 App 通信的 socket（服务端 listen） |
| `LAND_MODE` | `auto` | `auto\|direct\|blit` 调试强制帧路径 |
| `LAND_LOG` | `info` | `error\|info\|debug\|proto` |

## 排障

`wl-android doctor`（容器）与 App 内调试页：自检 socket 权限、协议版本、caps、
fd 往返、Vulkan 能力，并输出延迟/帧率统计。跨端问题按日志中的 `serial` 对齐定位。

# STATUS.md — wl-android 项目状态

> 最后更新：2026-07-29
> 状态：服务端完成。KWin 嵌套连接 + 帧到达 App ✅。App 待开发。

## 总体进度

| 里程碑 | 组件 | 状态 |
|---|---|---|
| M0 | 真机探测件 | ✅ |
| M1 | 协议 crate (`wl-android-common`) | ✅ |
| M2 | Smithay 服务端骨架 | ✅ |
| M3 | App 连接 + 握手 + 帧循环 | ✅ |
| M4 | 触摸注入 | ✅ |
| M5 | 动态配置（旋转/分辨率/刷新率） | ✅ |
| M6 | dmabuf handler + blit + AHB handle | ✅ |
| M7 | 性能收口 + Magisk 模块 | ✅ |
| — | 审计修复 (C1-C4, H1-H4) | ✅ |
| — | KWin 嵌套协议补齐 + dispatch 修复 | ✅ |
| **App** | Android 客户端 (Kotlin + Rust JNI) | ❌ 未开始 |

## 关键里程碑：KWin 端到端帧到达 ✅ (2026-07-29)

```
KWin nested → wl-android → land.sock → mock-client → Frame(serial=1, 3392x2400) → Ack
```

### 排错历程
| 步骤 | 错误 | 修复 |
|---|---|---|
| 1 | KWin "wl_compositor isn't supported" | `dispatch_clients()` 从未被调用 — Wayland 消息全卡在 socket buffer |
| 2 | 同上 | `flush_clients()` 缺失 — 初始 globals 未推送到客户端 |
| 3 | `wl_compositor v5` | 升级到 `CompositorState::new_v6()` |
| 4 | KWin 逐个报缺协议 | 依次添加 `single_pixel_buffer` / `viewporter` / `presentation` / `fractional_scale` / `content_type` / `alpha_modifier` / `pointer_constraints` |
| 5 | `wl_seat` not found | `new_seat()` → `new_wl_seat(&dh, ...)` — 前者只创建逻辑对象，后者才注册 Wayland global |

### 已注册的 Wayland 协议

`wl_compositor(v6)` `wl_subcompositor` `wl_shm` `wl_seat` `wl_output` `xdg_wm_base`
`zwp_linux_dmabuf_v1` `wp_single_pixel_buffer_manager_v1` `wp_viewporter`
`wp_presentation_time` `wp_fractional_scale_manager_v1` `wp_content_type_manager_v1`
`wp_alpha_modifier_v1` `zwp_pointer_constraints_v1`

## 测试状态

| 层 | 测试数 | 状态 |
|---|---|---|
| `wl-android-common` unit | 35 + 1 ignored | ✅ |
| `wl-android` unit | 14 | ✅ |
| `m0-probe` (device) | — | ✅ 已完成 |
| `m0-socket-smoke` (device) | — | ✅ SCM_RIGHTS 双向验证 |
| `mock-client` smoke (dev) | — | ✅ HELO→CONF→Touch→Frame→Ack |
| **KWin + mock-client (device)** | — | **✅ Frame serial=1 3392x2400 → Ack** |
| **KWin 帧到达 App (device)** | — | **✅ serial=1..2, 全链路** |

## 容器环境状态

| 项目 | 状态 |
|---|---|
| 设备 | OnePlus Pad 3 (OPD2413), Snapdragon 8 Elite |
| 容器 | Droidspaces Arch, aarch64 |
| KGSL | ✅ `/dev/kgsl-3d0` |
| Mesa | ✅ 26.2.0-5 (turnip A830 KGSL) |
| Vulkan 扩展 | blit only — `VK_EXT_external_memory_dma_buf` ❌, AHB ✅ |
| KWin | ✅ 嵌套模式连接成功，commit 帧 |
| `wl-android run` | ✅ KWin 连接 → new_toplevel → surface commit ×4 |
| `mock-client` | ✅ Frame(s1) → Ack 闭环 |
| LAND_SOCKET 默认 | `$XDG_RUNTIME_DIR/wl-android/land.sock`（`/tmp/wl-android/land.sock`）|

## 质量指标

| 指标 | 状态 |
|---|---|
| `cargo build` | ✅ |
| `cargo test` | ✅ 49/49 |
| `cargo clippy -D warnings` | ✅ clean |
| protobuf roundtrip (proptest) | ✅ |
| golden bytes (insta) | ✅ |
| fd 计数守卫 | ✅ (需 `--test-threads=1`) |

## 代码结构

```
wl-android/
├── crates/
│   ├── wl-android-common/              # 协议定义 + 测试基建
│   └── wl-android/                     # Smithay 合成器
│       └── src/
│           ├── main.rs                 # CLI + calloop 事件循环
│           ├── state.rs                # WlState: 所有协议 handler
│           ├── frame_router.rs         # 帧路由器状态机（7 tests）
│           ├── frame_router/tests.rs   # 8 tests
│           ├── transport.rs            # SCM_RIGHTS 传输层
│           ├── app_link.rs             # land.sock 会话管理
│           ├── comp/
│           │   ├── mod.rs
│           │   └── dmabuf.rs           # DmabufHandler + v4 feedback
│           ├── touch.rs                # TouchMessage→wl_touch 注入
│           ├── blit.rs                 # ash Vulkan blit 骨架
│           ├── ahb_handle.rs           # native_handle_t 解析 (GZ-001)
│           ├── doctor.rs               # 诊断工具
│           └── bin/mock-client.rs      # 容器内测试二进制
├── m0/                                 # 探测件
├── magisk-module/                      # module.prop + service.sh + sepolicy.rule
├── milestones/                         # M2-M7 验证脚本 + 真机测试计划
├── scripts/
│   ├── build-all.sh                    # 全量构建
│   ├── container-probe.sh              # 容器环境诊断
│   ├── m0-build.sh                     # M0 交叉编译
│   ├── soak.sh                         # 1h 浸泡监控
│   └── mock-app.py                     # Python mock 客户端
├── docs/
│   ├── DESIGN.md                       # 协议/状态机/时序图/API 设计
│   └── AGENTS.md                       # 三角色协作协议
├── BOUNDARIES.md                       # 边界约束 + 灰区登记
├── PERFORMANCE_BOUNDARIES.md           # 性能约束
├── STATUS.md                           # 本文件
└── README.md                           # 项目总览
```

## 已修复的审计问题 (2026-07-20)

| # | 严重度 | 问题 | 修复 |
|---|---|---|---|
| C1 | 🔴 | TBUF + AHB native_handle 二段消息无法接收 | `transport.rs` 新增 `recv_raw()` / `send_raw()` |
| C2 | 🔴 | FrameRouter 没有 buffer_id 注册表 | 新增 `registered: HashMap` + AppConnected 清理 + `has_fds` 字段 |
| C3 | 🔴 | `client_compositor_state` 每次 `Box::leak` | 改用 `thread_local! RefCell` |
| C4 | 🔴 | Commit handler 硬编码占位值 | 使用 surface hash 作为 buffer_id |
| H1 | 🟡 | 无协议版本协商 (H-03) | `do_handshake()` 检查 `conf.protocol_version` |
| H2 | 🟡 | 无模式选择 (H-04) | 新增 `SlotRegistration` 模式 + blit/direct 选择 |
| H4 | 🟡 | RouterAction 返回值被丢弃 | 新增 `dispatch_router_actions()` + 所有调用点消费 |

## App 实现待办 (Android 客户端)

### App 必须解 (写 App 时处理)

| # | 问题 | 影响 |
|---|---|---|
| A1 | **AHB 分配 + fd 提取 JNI 桥** | App blit 模式 slot 注册不可用 |
| A2 | **App 侧协议镜像** | App 无法和服务端通信 |
| A3 | **GPU 同步** (turnip blit↔App present) | 花屏/撕裂风险 |
| A4 | **cargo-ndk + Gradle 构建链** | APK 编译不通过 |

### App 可选修

| # | 问题 | 影响 |
|---|---|---|
| A5 | 帧节拍未接入 refresh_millihz (O-01) | KWin 不以正确刷新率渲染 |
| A6 | `wl_output.done` 发送缺失 (H5) | 部分 output 变更被忽略 |
| A7 | `touch.rs` unsafe 指针拆分 (M4) | UB 风险（calloop 单线程） |

### App 架构（已定稿）

- Kotlin：SurfaceView + DisplayListener + MotionEvent 薄壳
- Rust JNI (cdylib)：socket I/O + Vulkan 渲染 + 协议 + AHB 管理
- 渲染模式：blit-only（Adreno 830）
- JNI 边界：NativeBridge 单类，7 个 external fun
- 构建：cargo-ndk + Gradle (org.mozilla.rust-android-gradle)

详见上方 `## Android App 架构草案` 讨论记录。

## 下一步

1. ~~容器内验证 mock-client~~ ✅ 已完成 — Frame 到达 App
2. **开始 Android App 实现**：A1（AHB JNI 桥）→ A2（App 侧协议镜像）→ A4（构建链）→ A3（GPU 同步）
3. App 可选修：A5（帧节拍）→ A6（wl_output.done）→ A7（touch.rs）

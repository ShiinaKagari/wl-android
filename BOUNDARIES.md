# BOUNDARIES.md — wl-android 项目边界约束

> 本文件是项目的最高约束，任何代码、依赖、设计变更必须先通过本文件的审查。
> 与本文件冲突的实现一律拒绝合入。

## 0. 核心原则：透明中间层（Transparent Middle Layer）

**wl-android 是一个透明中间层：**

- 对合成器（KWin/Weston/Hyprland）表现为一个**规范合规的标准 Wayland 合成器（server 角色）**；
- 对 Android 系统表现为一个**普通应用（untrusted_app 域）**；
- 其全部正确性只能建立在**公开契约**之上；
- **不得以任何形式介入第三方代码的执行（零 hook）**。

"影响第三方行为"的唯一正当手段是**协议内协商**（例如通过 dmabuf feedback
格式表约束客户端选择 linear 格式、通过 `xdg_toplevel.configure` 驱动尺寸），
这是 Wayland 协议赋予 server 的标准能力，不属于 hook。

## 1. 允许清单（公开契约）

| 类别 | 内容 |
|------|------|
| Wayland | 以 server 角色实现标准协议：`wl_compositor` `wl_subcompositor` `wl_shm` `wl_output` `wl_seat` `xdg_wm_base` `zwp_linux_dmabuf_v1`(v4 feedback)；协议内协商（格式表、configure、frame callback 节拍） |
| Vulkan | 公开扩展：`VK_EXT_external_memory_dma_buf`、`VK_KHR_external_memory_fd`、`VK_ANDROID_external_memory_android_hardware_buffer` 等 |
| Android | SDK / NDK 公开 API（含 `AHardwareBuffer_*` 全部公开函数）；JNI |
| Linux | POSIX 与稳定内核 UAPI：Unix socket（`SOCK_SEQPACKET`）、`SCM_RIGHTS`、dmabuf、memfd |
| 配置 | 环境变量（`WAYLAND_DISPLAY` 等标准接口）；Magisk 官方机制内的**系统配置**（sepolicy 规则、目录创建） |
| 依赖库 | 以库的公开 API 正常链接使用（使用库 ≠ 修改第三方代码） |

## 2. 禁止清单（hook 行为，零容忍）

- ❌ `LD_PRELOAD`、so 注入、`ptrace`、inline / GOT / PLT hook
- ❌ 二进制 patch、修改任何第三方源码后重编译分发
- ❌ 链接私有符号；Android 隐藏 API（含反射绕过 / `libbinder` 私有接口）
- ❌ 读写其他进程内存
- ❌ 依赖合成器在协议规范之外的内部实现行为
- ❌ Magisk 模块中的代码注入或系统文件替换（仅允许配置类操作）
- ❌ patch / fork 合成器、Mesa、内核、Wayland 库
- ❌ 依赖任何合成器插件 API

## 3. 灰区登记表（非公开契约但非 hook 的依赖）

新增灰区项必须：单独模块隔离 + 契约测试 + doctor 自检 + 在此登记后方可使用。

| 编号 | 依赖内容 | 性质说明 | 隔离与防护 | 使用范围 |
|------|----------|----------|------------|----------|
| **GZ-001** | ① gralloc handle 内含 dmabuf fd（gralloc4 / Android 12+ 事实标准）② libcutils `native_handle_t` 线格式（`AHardwareBuffer_sendHandleToUnixSocket` 的发送格式：numFds/numInts/ints + SCM_RIGHTS） | 非 hook：仅解析发送到自有 socket 的字节流，不介入任何第三方进程；发送端使用的是公开 NDK API | 隔离到 `crates/wl-android/src/ahb_handle.rs` 单一模块；契约测试；doctor 启动自检，解析异常时**明确报错**而非静默 | 仅 blit fallback 路径 |

当前灰区总数：**1**。除登记项外，全项目为 100% 公开契约。

## 4. 依赖准入标准

新增任何依赖须同时满足：

1. 维护活跃（近 12 个月有发布或提交）；
2. 生态广泛使用；
3. 许可证为 MIT / Apache-2.0 / BSD 系（App 侧禁止 GPL 传染）；
4. 仅通过公开 API 使用（不 vendoring 后修改）。

已批准依赖：Smithay、calloop、wayland-server/client、nix、zerocopy、drm-fourcc、
ash、ash-window、raw-window-handle、jni、ndk/ndk-sys、thiserror、tracing、
android_logger、insta、proptest、cargo-ndk。

## 5. 范围边界（项目不负责）

- ❌ 安装 / 配置 / 排查 Mesa 驱动（用户环境已具备 turnip/freedreno，见 README 环境要求）
- ❌ 支持 proot 环境
- ❌ 使用 OpenGL ES（App 渲染仅 Vulkan）
- ❌ App 层面申请 root 权限
- ❌ 引入消息队列 / IPC 框架（仅裸二进制 socket 协议）
- ❌ 修改 Wayland 协议或发明私有 Wayland 扩展

## 6. 兼容性承诺

任何**规范合规**的 Wayland 嵌套合成器客户端都应能工作。验收矩阵：

| 合成器 | 状态 |
|--------|------|
| KWin (Plasma, `startplasma-wayland`) | 主目标 |
| Weston（嵌套模式） | 必须通过 |
| Hyprland（嵌套模式） | 必须通过 |

若某合成器依赖了我们未实现的**标准**协议，补实现；绝不以合成器专有方式绕过。

## 7. 变更流程

- 修改"禁止清单"：不允许。
- 新增灰区项：必须先在本文件登记（编号 GZ-xxx）并说明隔离方案。
- 新增依赖：按第 4 节标准审查，通过后追加到已批准列表。

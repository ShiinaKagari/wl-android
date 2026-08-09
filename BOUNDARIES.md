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
| Vulkan | **当前 v3 架构不使用 Vulkan**：server 侧 dmabuf fd 零拷贝转发（无 GPU blit），App 侧 CPU 渲染（`ANativeWindow_lock`）。blit 时代的扩展清单（swapchain / AHB / external_fence / external_semaphore / android_surface 等）已随 v2 管线删除（commit b3491f4） |
| Android | SDK / NDK 公开 API（含 `AHardwareBuffer_*` 全部公开函数）；JNI |
| Linux | POSIX 与稳定内核 UAPI：Unix socket（AF_UNIX `SOCK_STREAM` + u32 长度前缀 + `SCM_RIGHTS`）、dmabuf、memfd |
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
- ❌ 非 git 方式的源码同步进测试后端（tarball / 手工拷贝源码进容器）；源码进入测试后端只能走 `git pull`

## 3. 灰区登记表（非公开契约但非 hook 的依赖）

新增灰区项必须：单独模块隔离 + 契约测试 + doctor 自检 + 在此登记后方可使用。

| 编号 | 依赖内容 | 性质说明 | 隔离与防护 | 使用范围 |
|------|----------|----------|------------|----------|
| **GZ-001** | 跨进程 dmabuf fd 传递：KWin commit 提取的 dmabuf fd 经 `SCM_RIGHTS` 零拷贝转发给 App，App `mmap` 后 `ANativeWindow_lock` CPU 呈现（依赖 Linux dma-buf UAPI，fd 有效性依赖合成器遵守 Wayland dmabuf 契约） | 非 hook：仅按标准 Wayland 协议转发合成器提交的 fd，不介入任何第三方进程 | 隔离到 `crates/wl-android/src/comp/dmabuf.rs`（feedback/格式）+ `state.rs`（commit 提取/dup 转发）；fd 生命周期由 FdCountGuard 契约测试覆盖；doctor 启动自检 socket 目录/GPU 设备，异常时**明确报错**而非静默 | dmabuf 零拷贝主路径（fd dup 转发）+ SHM 回退（`frame_mem.rs` memfd 拷贝） |

当前灰区总数：**1**。除登记项外，全项目为 100% 公开契约。

## 4. 依赖准入标准

新增任何依赖须同时满足：

1. 维护活跃（近 12 个月有发布或提交）；
2. 生态广泛使用；
3. 许可证为 MIT / Apache-2.0 / BSD 系（App 侧禁止 GPL 传染）；
4. 仅通过公开 API 使用（不 vendoring 后修改）。

已批准依赖：Smithay、calloop、wayland-server/client、nix、zerocopy、drm-fourcc、
jni、ndk/ndk-sys、thiserror、tracing、android_logger、insta、proptest、cargo-ndk
（ash、ash-window、raw-window-handle 已随 blit/swapchain 管线删除，从清单移除）。

## 5. 范围边界（项目不负责）

- ❌ 安装 / 配置 / 排查 Mesa 驱动（用户环境已具备 turnip/freedreno，见 README 环境要求）
- ❌ 支持 proot 环境
- ❌ 使用 OpenGL ES（App 渲染仅 CPU：`ANativeWindow_lock` 像素拷贝，无 GPU API）
- ❌ App 层面申请 root 权限
- ❌ GUI 进程（server / KWin / Plasma）以 root 运行（必须为普通用户，见 §8.2）
- ❌ 引入消息队列 / IPC 框架（仅裸二进制 socket 协议）
- ❌ 修改 Wayland 协议或发明私有 Wayland 扩展
- ❌ 修改测试后端的环境变量、文件权限、系统参数（见 §8.1 测试后端不可变原则）

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

## 8. 环境拓扑与代码流转

| 端 | 定义 | 职责边界 |
|---|---|---|
| **开发机** (dev machine) | 当前主开发环境（运行 agent 的机器），git 仓库在此维护 | **代码改动只允许在此进行**；`git push` 的唯一来源 |
| **测试后端** (test backend) | 安卓设备上的容器（Droidspaces `--name=arch`） | server（wl-android）构建/运行/日志、Plasma/KWin 会话、doctor、soak；**除 App 测试外的一切** |
| **测试前端** (test frontend) | 安卓机本身（设备 376627b8） | **仅 App 测试**：安装、启动、logcat 观察、触摸/键盘交互验证 |

**源码流转只走 git**：开发机 `git push` → 测试后端 `git pull` 后构建/运行。
禁止 tarball / 手工拷贝等非 git 源码同步（见 §2 禁止清单）。部署产物（编译出的二进制）
例外：可按既有机制经 bind mount（`/data/local/tmp/wl-android` ↔ `/run/wl-android`）传递，
但源码必须走 git。

### 8.1 测试后端不可变原则（测试后端即标准环境）

**测试后端就是标准环境。** 以下改动对 agent 一律禁止，任何情况下不得执行：

- ❌ 修改测试后端的环境变量（容器配置、启动脚本 export、`/etc/environment`、`/etc/profile` 等一切生效路径）
- ❌ 修改测试后端的文件权限 / 属主 / 所有权（`chmod` / `chown` / `chattr` / `setcap` / ACL）
- ❌ 修改测试后端的系统参数（容器配置 `container.config`、`sysctl`、SELinux 策略、`setenforce`、`droidspaces` 启动参数、Magisk/sepolicy 规则）
- ❌ 为"修复运行问题"而临时改任何后端状态（`setcap` 二进制、`chown` 目录、改写启动脚本等）

**运行失败时的归因规则：**

- 若程序（server / Plasma / KWin / App）在标准环境下无法正常运行，
  **默认视为项目自身的问题**——优先排查 wl-android 代码、构建产物、协议实现、运行时假设；
- 环境相关怀疑（驱动、权限、配置）必须先以**只读方式验证**（查看状态、对比日志、在
  不修改任何状态的前提下诊断），确认是环境缺陷后**报告用户**，由用户决定是否调整环境；
- agent 不得自行以修改环境为手段"修复"问题。

> 例外：用户**明确、实时**指示的临时环境操作（如本次会话中用户指示的 `setenforce 0`），
> 仅在指示范围内执行，且操作前后必须向用户明示差异。

### 8.2 GUI 进程用户权限原则（符合 Linux desktop 规范）

**所有 GUI 程序禁止以 root 运行，必须以普通用户权限运行**，遵循 Linux desktop 规范：

- ✅ server（wl-android）、KWin、Plasma 及一切图形 / 桌面相关进程以非 root 用户运行
- ✅ 符合 freedesktop 规范：`XDG_RUNTIME_DIR=/run/user/<uid>`（0700、属主=运行用户）、
  `WAYLAND_DISPLAY`、`DBUS_SESSION_BUS_ADDRESS` 等标准环境由登录机制提供
- ✅ 涉及特权资源（GPU 设备访问、capabilities）时，通过**标准机制**解决
  （容器/登录配置、用户组、设备节点既有权限），而非以 root 运行 GUI 绕过
- ❌ 禁止以 root（或提权）启动任何 GUI 进程作为"让它跑起来"的手段
- ❌ 禁止为 GUI 进程授予文件级 capabilities（`setcap`）作为运行前提

**验证方式**：运行中的桌面栈进程（server / kwin_wayland / plasmashell 等）
`ps -o user=` 必须为非 root 用户；`XDG_RUNTIME_DIR` 必须指向 `/run/user/<uid>` 且属主正确。

### 8.3 桌面会话启动唯一入口（强制）

**所有 plasma / kde 相关程序（plasmashell、kwin_wayland、plasma_session、ksmserver、
kactivitymanagerd 等）严禁单独启动**——必须且只能通过 `startplasma-wayland` 命令
启动整个会话（先起 wl-android server，再 `startplasma-wayland`）。

- ❌ 禁止单独运行 / strace / gdb / 手动拉起任何 KDE 组件（会脱离会话的
  D-Bus 总线、systemd user 服务联动与正确环境，导致 plasmashell 卡死或崩溃）
- ❌ 禁止用 `droidspaces run` 直跑 plasmashell/kwin 等做"测试"
- ✅ 观察 KDE 组件状态用 journald / 日志文件 / `ps` 等只读手段
- ✅ 完整启动/重启桌面栈的唯一入口：`tools/start-stack.sh`（内部按
  server → startplasma-wayland 顺序执行）


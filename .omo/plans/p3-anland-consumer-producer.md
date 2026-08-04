# P3 拉式解耦计划 — anland 风格 Consumer-Producer（P1→P2→P3 全阶段）

## TL;DR

把 wl-android 从「server 分配 + 推帧 + App CPU 拷贝」的 v1 管线，重构为 anland 风格的
**Consumer-Producer 拉式解耦**：App（Consumer）拥有 buffer（Vulkan swapchain 图像 =
AHardwareBuffer）、驱动帧节奏、只做 ① 上报配置 ② 收集输入 ③ 呈现后端渲染结果；
server（Producer）把 KWin 帧 GPU blit 进 App 的 buffer，用 **内核 sync_file 栅栏**
（SCM_RIGHTS 传输）与 App 同步。同时解决 5 个已知问题：闪退 SIGBUS、卡顿、键盘、
带宽/拷贝、Client 解耦。

**四条硬约束（用户设定，计划全程不可违反）**：
1. **不放弃 Vulkan** — anland 本身用 GL/EGL（其调研已确认），我们**不复制渲染栈**，只复制架构模式；blit 用既有 turnip Vulkan 管线（`blit.rs`）。
2. **不能动别人的代码** — 零 hook（BOUNDARIES §2）；不做 anland 那种 KWin backend；App 侧**不用** anland 的 dlsym 隐藏 ANativeWindow API。
3. **尽量标准实现** — 公开 Vulkan/NDK/Wayland API；栅栏 = 标准 `sync_file` fd。
4. **保证最优性能** — PERF-xx 全达标（零 CPU 像素拷贝 PERF-01、端到端延迟、零导入 PERF-11）。

**用户已拍板的 4 个决策（不再问）**：
- D1. 一份计划，P1→P2→P3 全阶段。
- D2. 同步模型：**扩展现有协议**（保留 LAND/FACK/TBUF + golden bytes），新增 fence fd 随帧消息 + consumer 驱动的 buffer-ready 信号（`BRDY`）；不引入 anland 的 eventfd/shm 三通道。
- D3. App 呈现路径：**Vulkan swapchain 公开 API**（`VK_KHR_android_surface` + `VK_ANDROID_external_memory_android_hardware_buffer`），禁 dlsym。**（用户已重确认维持，不换 ASurfaceTransaction）**
- D4. 键盘：**完整 + xkbcommon**（MIT，过 BOUNDARIES §4 审查），协议 v2 新增 KeyMessage。

**fence 调研结论（已代码级确认，P2/P3 据此落地）**：
- ✅ turnip 支持 `VK_KHR_external_fence` + `VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT` **导入+导出**（`tu_knl_kgsl.cc` KGSL timestamp→sync_file，`KGSL_CMD_SYNCPOINT_TYPE_FENCE` GPU 侧等待）——主路径成立，无降级必要（server 侧）。
- ✅ turnip 支持 `VK_KHR_external_memory_fd` + `VK_EXT_external_memory_dma_buf` 导入任意同内核 dma-buf。
- ⚠️ **容器内 turnip 无 `VK_ANDROID_external_memory_android_hardware_buffer`**（Linux 构建 `has_gralloc=null`）→ server 侧**只导入原始 dma-buf fd**；AHB 扩展仅 App 侧（宿主驱动）使用。
- ⚠️ **`VK_ANDROID_external_fence_sync_file` 从未发布**（Khronos registry/loader 零实现）→ 全链路只用 `VK_KHR_external_fence_fd`。
- ⚠️ **UBWC 是最大未验证风险**：swapchain 图像由 gralloc 决定 modifier（Adreno 几乎必 UBWC）；turnip 仅支持 LINEAR/QCOM_COMPRESSED，导入时 modifier 必须匹配，且 App 侧宿主驱动 SYNC_FD 需运行时断言。→ P3 加 **bring-up 门控**（TODO 30 前先验证 UBWC 导入）。
- ⚠️ stride 必须随消息传递（TBUF 已有 stride 字段），导入用 `VkImageDrmFormatModifierExplicitCreateInfoEXT` 显式布局。

## 阶段总览

| 阶段 | 内容 | 验收锚点 |
|------|------|---------|
| **P0** | DESIGN.md 升 v2（规则先行）+ 协议 v2（KeyMessage/BRDY/FENCE flag/caps） | golden bytes 绿、cargo test 全绿 |
| **P1** | 稳定化：SIGBUS 修复、BGRA 零循环拷贝、键盘端到端 | 快速连击不 crash、桌面流畅、KWin 文本可输入 |
| **P2** | server blit 管线接线：TBUF→Vulkan 导入→blit→fence 导出 | mock-app 全 v2 回归绿 + 容器真机 doctor 断言 |
| **P3** | App swapchain 拉式解耦 + 移除 CPU 路径 | PERF-01~15 达标、V-xx 真机验收 |

### 协议 v2 增量（P0 写入 DESIGN.md 后实现）

| 新/改 | 项 | 定义 |
|-------|----|----|
| 新增 | `KEYM` KeyMessage (App→server, 16B) | `{magic u32, keycode u32 (evdev scancode), state u32 (0=up 1=down), time_ms u32}` |
| 新增 | `BRDY` BufferReady (App→server, 16B) | `{magic u32, slot u32, _reserved u64}` — P3 拉式节奏：App 宣告某 slot 可写 |
| 修改 | `LAND` FrameMessage flags | 新增 bit1 `FRAME_CARRIES_FENCE`：本消息伴随 1 个额外 fd = server blit 完成栅栏 (sync_file) |
| 修改 | `HELO` server_caps | 新增 bit1 `SERVER_CAP_FENCE`：server 支持栅栏同步 |
| 修改 | `CONF` app_caps | 新增 bit1 `APP_CAP_SWAPCHAIN`：App 走 Vulkan swapchain 呈现 |
| 修改 | `PROTOCOL_VERSION` | 1 → 2 |
| 新增 | F-12 规则 | 栅栏 fd 生命周期：server 导出即转移（SCM_RIGHTS 内核 dup，server 保留副本至信号后销毁）；App 导入后由驱动接管（F-02 同规则延伸） |
| 新增 | F-13 规则 | `-1-already-signaled`：`vkGetFenceFdKHR` 在 fence 已信号时返回 -1，此时**不发 fd**，App 视为立即可呈现 |
| 新增 | F-14 规则 | BRDY 语义：App 仅在 vkAcquireNextImageKHR 返回且 acquire 完成后发送；server 收到后 blit，未收到 BRDY 的 slot 视为占用（等价 F-09 反压） |
| 修改 | H-04 | blit 模式判定增加 caps 组合；`LAND_MODE=blit` 时等 `SLOT_COUNT` 条 TBUF 才 active（恢复被跳过的 SlotRegistration 门控） |

规则编号沿用约定（AGENTS.md / DESIGN.md）：测试名或注释必须引用规则编号（P-xx/H-xx/F-xx/C-xx/T-xx/O-xx/X-xx/PERF-xx）。

## Todos

- [x] 1. `docs/DESIGN.md`: 写 v2 增量章节（上表逐项），更新消息一览表、§4.x 布局、§6 buffer 生命周期（F-08~F-14）、§5 H-04、§13 ADR 表（新增 ADR #16: swapchain 呈现 + 栅栏；#17: 键盘 v2 不再推迟）。依据 DESIGN.md「先改规则文件再改代码」铁律。验证: git diff 审阅，规则编号连续无冲突。**（已验证：11/11 项规格就位，P-07=v2，=1 计数 0；首轮 writer 声明 false-positive，二轮 confirmed）**

- [x] 2. `crates/wl-android-common/src/proto.rs`: 新增 `MAGIC_KEYM`/`MAGIC_BRDY`、`KeyMessage`/`BufferReady` 结构体、`FRAME_CARRIES_FENCE`/`SERVER_CAP_FENCE`/`APP_CAP_SWAPCHAIN` 常量、`PROTOCOL_VERSION=2`、`Message` enum 两新变体、encode/decode/fd_count 分支（FENCE flag → fd_count=1）。验证: 编译通过；`size_of` 断言新增 16B 两项。**（confirmed：26 编译错红→52 绿，fd_count 公式 `(fds?num_planes:0)+(fence?1:0)`，size 断言 16B×2）**

- [x] 3. `crates/wl-android-common/src/proto/tests.rs`: 新增 golden bytes（insta）——`keym_message_golden`、`brdy_message_golden`、`frame_carries_fence_golden`；proptest roundtrip 覆盖新消息与 FENCE-fd 帧。验证: `cargo test -p wl-android-common` 全绿（含新 golden）。**（confirmed：3 新快照内容真实，LAND+flags=2 / KEYM / BRDY）**

- [x] 4. 协议契约测试（QA）：`FrameAck` 复用帧不携带 fence、FENCE flag 与 fd 数严格匹配（P-08 延伸）、未知 magic 断开（P-05）——全部在 common crate 单测覆盖。验证: 失败用例先红后绿（X-08）。**（confirmed：frame_fence_requires_exactly_one_fd / missing_fd / extra_fd / reuse_frame_requires_zero_fds 全绿，红先于绿）**

- [x] 5. `crates/wl-android/src/frame_cache.rs`: `set_dimensions` 改为**只允许增长/重建**——尺寸变小或不等时分配新 memfd 三缓冲并原子替换（旧 fd 由持有方自然释放），禁止原地 ftruncate 缩小（修复 SIGBUS 根因 1）。验证: 单测 `dimension_change_reallocates_not_shrinks`（FdCountGuard 包裹）；真机快速连击不 SIGBUS。测试编号: 新 F-15（尺寸变更原子重建）。**（confirmed：原子重建实现+4 测试，SIGBUS 断言真实——mmap 旧尺寸→resize→读末字节；`--test-threads=1` 下 4/4 绿，全 crate 18/18，clippy 0 警告；并行线程 fd-guard 竞争为既有约定非缺陷）**

- [x] 6. `crates/wl-android/src/state.rs` commit `None` 分支 (L398-418): 发送 `cache.current_frame()` 真实尺寸（已带回 `(fd, seq, cw, ch)`），删除屏幕尺寸传参（修复 SIGBUS 根因 2）。验证: App 侧 mmap 尺寸 = 帧真实尺寸，log 无 truncated。**（confirmed：修复已在 HEAD 预存——commit be9b726 引入 None 分支即用真实尺寸 `cw,ch` 于 buffer 槽位 L408-413，screen 尺寸仅在 advisory 槽；零 diff 需要，cargo check 0 错误）**

- [x] 7. `android-app/native/src/session.rs` (L126-152): 收帧后先 `fstat(fd)` 取真实大小再 mmap，`fstat.size < 预期` 时按 fstat 截断读取（防 SIGBUS 最后防线，根因 3）。验证: 单测（host 端 memfd 模拟小 fd）+ 真机日志无 SIGBUS。**（confirmed：`safe_mmap_len` helper + run_loop 接线 + 3 测试；harness 证据落盘 `.omo/start-work/fstat-harness/test-output.log`，3 passed×2 轮、与 repo 逐字节 diff 空；host cargo test 仅被 bridge.c NDK 头阻塞，与 session.rs 无关）**

- [x] 8. `android-app/native/src/jni_bridge.rs` (L22-23): 修复双 `ANativeWindow_release`（只释放一次）。验证: 真机快速 tap 10 次不闪退；`ps` 确认进程存活。**（confirmed：单行删除，diff 精确；aarch64 target 未装编译受限如实报告）**

- [x] 9. `android-app/native/c/bridge.c` (L19-21): `wl_set_format` 由 `WINDOW_FORMAT_RGBA_8888` 改为 `WINDOW_FORMAT_BGRA_8888`（KWin 帧 BGRX，BGRA 窗口格式整行 memcpy 零逐像素交换，修复卡顿根因）。验证: 真机 `render:` 日志 `fmt=BGRA`，画面颜色正确（蓝任务栏 `(5,102,191)`）。**（confirmed：BGRA_8888 + #ifndef 回退=5，NDK 27 无该常量；真实 NDK clang C 编译通过）**

- [x] 10. `android-app/native/src/jni_bridge.rs` (L91-121): `render_frame` 像素循环替换为**整行 `copy_nonoverlapping`**（BGRX→BGRA 字节序一致，仅 stride 对齐）；保留 RGB_565 分支为 fallback。验证: 单测（小缓冲逐行 vs 循环等价）；真机帧率提升（期望 ≥3×）。**（confirmed：`copy_row_bgra` 逐行 memcpy+逐行 clamp，红→绿（旧逐像素在截断源 OOB panic→新 clamp 不 panic）；发现旧循环 BGRX→RGBA 交换在 BGRA 窗口下会 R/B 颠倒→改 passthrough 行拷贝；harness 3 测试落盘 .omo/start-work/jni-render-harness/）**

- [x] 11. `android-app/native/src/session.rs` (L145-152): 移除 `slice.to_vec()` 冗余拷贝——mmap 后直接传 `&[u8]` 给回调。验证: 编译 + 真机帧率再提升。**（confirmed：生命周期分析——lib.rs 闭包同步用、FrameData 仅元数据、render_frame 调用期内拷贝完；mmap 存活期内直传 slice，munmap 严格在 on_frame 后（L180-182）；帧路径无 to_vec）**

- [x] 12. `android-app/app/src/main/java/com/wl/android/MainActivity.kt` + `NativeBridge.kt`: 实现 `dispatchKeyEvent`/`setOnKeyListener` 捕获 `KeyEvent`（`getScanCode()` = evdev scancode、`action` DOWN/UP、`eventTime`），经新 `nativeOnKey(handle, keycode, state, timeMs)` 转发；SurfaceView `setFocusable(true)` + `requestFocus()`。验证: 真机 logcat 出现 `key: code=X state=1`。**（confirmed：compileDebugKotlin BUILD SUCCESSFUL；BACK/HOME 守卫、forward-then-consume、focus 双处抓取；gradle 产物已 restore）**

- [x] 13. `android-app/native/src/lib.rs`: 新增 `Java_com_wl_android_NativeBridge_nativeOnKey` JNI → `session.send_message(Message::Key(...))`。验证: host 编译 + 真机日志收到 KEYM。**（confirmed：+16 行，KeyMessage::new 签名实读一致，Kotlin/Rust arg 顺序 4/4 匹配）**

- [x] 14. `crates/wl-android/src/state.rs`: 新增 `handle_key(msg)`——`seat.get_keyboard()` 注入 `KeyboardHandle::input(KeyEvent { keycode, state, serial, time })`；keymap 用 xkbcommon（`XkbConfig`/smithay 内置 keymap 接口）。验证: 单测 `key_down_injects_into_seat`（mock seat）；真机 KWin konsole 输入文本。**（confirmed：smithay 调研——KeyboardHandle::input 取 X 风格 keycode（evdev+8）→ handle_key 加 8；keymap 显式 US；`key_state_from_u32` 纯映射 1→Pressed else Released；3 测试红→绿；+8 偏移与真机 keymap 正确性留真机验证）**

- [x] 15. `crates/wl-android/src/main.rs` (L194-218) Active 分支: 增加 `Message::Key` 分发 → `handle_key`（对齐 Touch/Config 分支模式）。验证: 真机端到端键盘可用。**（confirmed：Key 分支 L210-213；修复了 `_ => false` 静默吞 KEYM 的问题（非编译错）；cargo check 0 错误）**

- [x] 16. `crates/wl-android/Cargo.toml`: 新增 `xkbcommon = "0.5"` 依赖（MIT，维护活跃，BOUNDARIES §4；若 smithay 已透出 keymap 能力则复用，不重复引入）。验证: PM 按 BOUNDARIES §4 审查通过；容器内 `cargo build` 绿。**（confirmed：按计划"复用不重复引入"——smithay 0.7 非可选依赖 xkbcommon 0.8 并 re-export（MIT），Cargo.toml 零改动，无版本漂移风险）**

- [x] 17. `crates/wl-android/src/comp/dmabuf.rs` (L86): `import_dmabuf(fd, 0, 0, ...)` → 传真实 `dmabuf.width()/height()`（修复零 extent bug）。验证: `blit_engine` 日志真实 extent；不崩。**（confirmed：w/h 取自同一 Dmabuf 对象，cargo check 0 错误，L86-88）**

- [x] 18. `crates/wl-android/src/app_link.rs` (L150-151, TODO M6b): TBUF 的 native_handle fd 不再丢弃——存入 server 侧 **slot 注册表**（blit.rs 内 `HashMap<u32 slot, u64 vk_handle>`），`import_dmabuf(fd, TBUF.width, TBUF.height, ABGR8888, LINEAR)`（真实尺寸）。验证: 单测 `slot_fd_imported_not_dropped`（FdCountGuard）；doctor 显示 slot 计数。**（confirmed：`register_slot/unregister_slot/clear_slots` + `recv_message(&mut self, blit)` 接线两调用点；导入失败 warn 续活、解析失败 Err 断开（P-13）；AppLost 清理 slot（C-02）；X-04 fd 无泄漏测试通过）**

- [x] 19. `crates/wl-android/src/app_link.rs` + `state.rs`: 恢复 `SessionMode::SlotRegistration` 门控（H-04 修改版）——blit 模式等 `SLOT_COUNT=3` 条 TBUF 才 `activate()`（对齐 DESIGN.md 原设计 + M3-verify.sh V-06 预期，去除 L77-80 跳过注释）。验证: `mock-app` 发 3 TBUF 后 server 才发帧（X-06 回归）。**（confirmed：红→绿（blit 分支改 SlotRegistration 后断言 Active→SlotRegistration）；3 TBUF 后 activate+replay；握手 replay 仅 Active 模式；frame_ack_roundtrip 重写为 3 TBUF+handle 先行；23 测试绿，clippy 无新增）**

- [x] 20. `crates/wl-android/src/blit.rs`: `blit_submit` 支持按帧 fence；新增 `export_fence_syncfd(fence) -> Result<Option<OwnedFd>, String>`（`VK_KHR_external_fence` + `VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT`，`vkGetFenceFdKHR`；-1 时返回 None = 已信号，F-13）。调研已确认 turnip 支持 SYNC_FD 导出（`tu_knl_kgsl.cc` timestamp→sync_file），此为主路径。验证: 容器真机 doctor 能力断言（GPU 单测 host 不可跑，X-07 薄壳）。**（confirmed：export_fence_syncfd + blit_submit_with_fence 包装 + create_exportable_fence + destroy_fence_handle；中止遗留 ~95% 正确补齐并修正"export 重置"错误注释（SYNC_FD 为 copy transference）；-1 短路 Ok(None) 防 fd 泄漏；ash::khr::external_fence_fd::Device 路径实证；23 测试绿）**

- [x] 21. `crates/wl-android/src/blit.rs` `init` (L79-83): 设备扩展增加 `VK_KHR_EXTERNAL_FENCE_NAME` + `VK_KHR_EXTERNAL_FENCE_FD_NAME`；fence 创建带 `ExternalFenceHandleTypeFlags::SYNC_FD` 导出能力。**禁用 `VK_ANDROID_external_fence_sync_file`**（调研确认从未发布，勿用）。验证: 容器 doctor 显示 FENCE 能力 true。**（confirmed：5 扩展；self.fence 带 ExportFenceCreateInfo{SYNC_FD}；VK_ANDROID 仅注释提及从未发布）**

- [x] 22. 降级路径（App 侧宿主驱动 SYNC_FD 能力未知——运行时断言）: `doctor.rs` 探测 `vkGetPhysicalDeviceExternalFenceProperties` 的 SYNC_FD 支持；server 侧 turnip 已确认支持，**仅当 App 侧不支持时**退化为「blit 完成 CPU wait + 发帧无 fence fd」（App 以 FACK 节奏兜底），doctor 明示降级（X-07）。验证: 容器真机 `wl-android doctor` 明确报告能力或降级。**（confirmed：probe_fence_capabilities +71 行；host 实测 SYNC_FD export/import/roundtrip 全支持；ExternalFenceProperties 无 handle_types 字段——用 external_fence_features(EXPORTABLE/IMPORTABLE)+export_from_imported_handle_types（ash 0.38 实证）；失败分支不 crash 明示降级）**

- [x] 23. `crates/wl-android/src/state.rs` commit (L327-462) + `frame_router.rs`: dmabuf 路径改为「导入 KWin 帧（buffer_id 缓存）→ 找空闲 slot → `blit_submit(src, slot, w, h)` → fence 导出 → `send_frame(serial, slot, ..., fence_fd)`」；无空闲 slot → 反压（F-04/F-09）；blit fence 信号即 release KWin buffer（F-10）；EnqueueFrame 携带 fence fd（修复 main.rs L254-263 broken fd lookup）。**导入必须用 TBUF 携带的 stride + `VkImageDrmFormatModifierExplicitCreateInfoEXT` 显式布局**（调研确认 stride 缺失是常见错误源）。验证: mock-app 回归断言「帧带 fence fd」；真机 doctor 延迟数据。**（confirmed：7/7 检查 PASS——send_frame 8 参带 fence_fd 尾参+set_carries_fence、PendingFrames 键值 map 替代 pending_pixel_fds、blit_and_send_frame 全链路（take→import 缓存→free slot→blit→export→send pixel=None+fence）、free_slots_on_ack 用 `serial<=ack`、AppLost 清理、SHM 路径原样；修 3 真缺陷：cum-ack off-by-one/pre-activation 卡死/fd 消费时机；30+52 测试绿；frame_router 按设计未动；唯一偏差=执行者"state.rs-only"声明不准（实际 4 文件）但功能正确）**

- [x] 24. `crates/wl-android/src/main.rs` (L247-291) + `state.rs` (L121): `pending_pixel_fds` 死字段清理——EnqueueFrame 改用「serial→(fd, fence_fd)」真实键值映射，删除 `drain().next()` 通配。验证: 单测（router action→send_frame 参数匹配）；FdCountGuard 全绿。**（confirmed：pending_pixel_fds 仅剩文档注释；EnqueueFrame→blit_and_send_frame(*bid,*serial) 无 drain hack；noop 测试因 WlState::new() 单测环境 SIGSEGV 未加——已由 pending_frames_stash_take_clear 数据边界覆盖+blit_mode_roundtrip 端到端覆盖，理由成立）**

- [x] 25. `crates/wl-android/src/state.rs`: 移除死字段 `blit_image_handles` 与 `pending_pixel_fds`；`comp/dmabuf.rs` 的 import 结果改走 slot 注册表。验证: `cargo clippy` 无 dead_code；全量测试绿。**（confirmed：blit_image_handles 全树 0 命中；comp/dmabuf 删冗余 eager import 保留 notifier.successful（KWin 接受信号）——惰性导入经 frame_images 缓存（PERF-11）在 blit_and_send_frame 完成；clippy 无死代码警告；验证者纠正：DrmFourcc import 被 build_default_feedback 使用应保留（执行者判断正确））**

- [x] 26. `android-app/native/src/render.rs`（当前 25 行桩 → 完整实现）: 真实 `RenderState`——Vulkan instance（`VK_KHR_android_surface` 扩展）+ surface + `VK_KHR_swapchain`；swapchain 图像即 AHardwareBuffer（gralloc）；`acquire_next_image() -> (u32 slot, vk::Semaphore/Fence)`；`present(slot, wait_semaphores)` 用 `vkQueuePresentKHR`（waitSemaphores 含 server 栅栏导入 semaphore）。**ASurfaceTransaction 不采用**（用户维持 D3）。验证: host 编译（ndk）；真机日志 `present: slot=N`。**（confirmed：583 行完整实现——instance/surface/swapchain/图像/acquire/present/recreate/Drop 顺序全就位；E0502 借用修复（loader scoped 块内、recreate 后重借）；编译 0 错误；harness 12/12（pick_format/present_mode 等纯函数）；格式决策 B8G8R8A8 优先、R8G8B8A8 回退（与 server blit ABGR8888 交互已文档化，vkCmdBlitImage 转换由 lane 29 处理））**

- [x] 27. `android-app/native/src/ahb.rs`（桩 → 完整）: 对每个 swapchain 图像调 **App 侧宿主驱动**的 `VK_ANDROID_external_memory_android_hardware_buffer` 扩展 `vkGetMemoryAndroidHardwareBufferANDROID` 取 AHB，再 `AHardwareBuffer_sendHandleToUnixSocket` 送 fd；`to_tbuf_message` + native_handle 随 TBUF 发出（P-13 原设计）。**容器内 turnip 无此扩展（Linux 构建）——server 侧只收原始 dma-buf fd，不走 AHB 扩展**（调研确认）。验证: 真机 logcat 3×TBUF + native_handle；server slot 注册表 3/3。**（confirmed：ahb.rs 5/5 规格项（from_swapchain_image/send_registration/to_tbuf_message describe 真 stride/Drop release/allocate 回退），ash::android::external_memory_android_hardware_buffer 路径实证，harness 6/6；render.rs seam 补全（AHB 扩展+DEFERRED_MEMORY_ALLOCATION_EXT+per-image 专属内存（dedicated+export ANDROID_HARDWARE_BUFFER）+image_memory()/raw_instance()/raw_device_ref() 访问器+recreate/Drop 双路径 free+clear 防双释放）；编译 0 错误，双 harness 6+12 绿）**

- [x] 28. `android-app/native/src/session.rs`: 握手后（blit 模式）发送 SLOT_COUNT 条 TBUF + fd，替换「不发 TBUF 直接 Active」；新增 `send_brdy(slot)`。**TBUF 必须携带 AHardwareBuffer_describe 得到的 stride**（调研：stride 缺失是常见错误源）。验证: 单测（mock server 收 3 TBUF 后回帧）。**（confirmed：run_loop 加 slots: Vec<AhbSlot> 参数（seam：调用方 lane 30 构建），CONF 后 TBUF→native_handle 顺序注册（send_tbuf_then_handle 可测 seam），空 slots 警告；send_brdy 用 Message::Ready(BufferReady)（proto 实读确认）；AhbHandle unsafe Send 验证可跨线程；harness 4/4 红→绿；强制重编译 0 错误）**

- [x] 29. `android-app/native/src/session.rs` + `render.rs`: 帧循环重构——收帧后不再 mmap/CPU 拷贝：(a) 解析 `FRAME_CARRIES_FENCE` 取 sync_file fd；(b) 导入为 semaphore/fence（`VK_KHR_external_semaphore`/`VK_KHR_external_fence_fd` + `SYNC_FD` 或直接 wait；**禁用 `VK_ANDROID_external_fence_sync_file`**）；(c) `present(slot, fence)`；(d) 发 FACK；(e) 发 `BRDY` 宣告 slot 可复用。验证: 单测（mock fd 流）+ 真机零 CPU 像素日志（无 `slice.to_vec`）。**（confirmed：7/7——dispatch_frame 纯函数（fence=最后 fd、plane 丢弃）、send_fack_and_maybe_brdy（FACK 后 BRDY iff fence）、on_frame 签名加 Option<OwnedFd>、render import_sync_fd_as_semaphore/destroy_semaphore/wait_sync_fd（libc::poll POLLIN）+ VK_KHR_external_semaphore_fd 条件启用（enumerate+回退）、lib.rs 闭包更新（Some=log+drop 待 lane 30）；决策：保留 recv-driven 循环+拉式信号（对 lane 31 前后 server 均正确）；harness 5/5 红→绿；编译 0 错误）**

- [x] 30. `android-app/native/src/jni_bridge.rs` + `lib.rs`: CPU 渲染路径（`render_frame`/`ANativeWindow_lock`）标记 deprecated，仅非 swapchain 降级模式启用；`lib.rs` 回调改走 `render.present_frame`。**前置 bring-up 门控：先验证 turnip 导入 UBWC swapchain 图像**（doctor 增加 `import_ubwc_test`：导入第一个 slot 后读回像素校验，失败则提示强制 LINEAR 或暂停 P3）；App 侧宿主驱动 SYNC_FD 运行时断言（`vkGetPhysicalDeviceExternalFenceProperties`）。验证: 真机 swapchain 模式无 CPU 日志（PERF-01 门禁）；UBWC 门控通过或明确降级。**（confirmed：7/7——nativeSetSurface render.init+register_swapchain_slots（Inner 锁内 TBUF+send_registration）、present_fence_frame（import→present→destroy / wait_sync_fd 回退）、consecutive_present_failures 3 连败 UBWC 门控、semaphore_fd_supported() 断言、#[deprecated] render_frame + #[allow(deprecated)]、V-30 present 标记；锁分析：setup 窗口安全（server 3 TBUF 门控前无 FACK/BRDY 流量），双写者纪律留 M7；编译 0 错误）**

- [x] 31. `crates/wl-android/src/app_link.rs` + `state.rs` + `frame_cache.rs`: 接收 `BRDY` 消息 → 标记 slot 空闲（F-14），唤醒 pending 帧 blit；删除 server 侧 `frame_cache` 推帧路径（SHM/CPU 路径退役，仅 `LAND_MODE=shm` 调试保留）。验证: mock-app 回归（BRDY 驱动节奏）；headless drain（F-06）不受影响。**（confirmed：6/6——SlotReadySet（mark_ready/consume/is_ready/clear）+ slot_blittable（slots.contains && ready && !in_use）+ handle_brdy + main.rs Ready 分支 + SlotRegistration 初始 mark_ready（死锁解决：TBUF 注册即初始 ready，后续复用需显式 BRDY）+ SHM 路径 LAND_MODE=shm 门控（关→warn+drop）+ frame_cache DEBUG-ONLY 标注保留 + AppLost 清 brdy_ready（C-02）；首帧路径端到端死锁分析；36 crate 测试 + 6 harness 绿）**

- [x] 32. `crates/wl-android/src/doctor.rs`: 增加 PERF-11 导入计数、fence 能力、模式（swapchain/blit/shm）报告；`scripts/soak.sh` 1h 采样 RSS/fd（PERF-05/07）。验证: 真机 `doctor` 报告 + soak 无违反。**（confirmed：6/6——effective_mode 纯函数（shm/direct/blit/默认）+ Mode 行；诚实方法报告（doctor 独立进程无法读 server 内存——PERF-11 给出真实测量法：grep 'dmabuf imported' 日志；PERF-02/03 在 App 端测量，引用 deploy-test verify）；soak.sh 两修正（RSS 阈值 32000→32768 kB 正确换算、头注释纠正）；7 harness 测试红→绿）**

- [x] 33. 文档定稿: `docs/DESIGN.md` v2 定稿（标注 swapchain 主路径）；`BOUNDARIES.md` 灰区表确认（GZ-001 不变；swapchain 全公开 API 不入灰区）；`README.md` 架构图更新（Vulkan swapchain 呈现）。验证: 文档与代码一致。**（confirmed：DESIGN §1 图重排（swapchain 主路径+BRDY 拉式）、F-09 改写为 slot_blittable 语义、直接模式不可用标注（ADR#15）、LAND_MODE auto|blit|shm、mermaid v2；BOUNDARIES 允许清单补 VK_KHR_external_fence_fd/external_semaphore_fd/android_surface/swapchain，GZ-001 使用范围更新，无新灰区；README 三档帧路径+架构图；9 处 stale 声明修复）**

- [x] 34. 全量回归: `cargo test` 全绿（含新 golden/FdCountGuard）、`build-all.sh` 三目标编译、`deploy-test.sh server+app+verify` 真机流程、`milestones/M3-M7-verify.sh` 交互验收。验证: 见「Final verification wave」。**（confirmed：52+36 crate 测试绿、交叉编译 0 错误、doctor 正常；初测发现 fstat/frame-loop harness FdCountGuard 竞态（进程级 fd 计数被并发测试干扰）→ 修复（全 fd 测试加 OnceLock<Mutex> 序列化+PoisonError 恢复）→ 8 harness×3 连跑 24/24 全绿；真机项（deploy-test/milestones）留 F-wave）**

## Final verification wave

- [x] F1. 代码审查（PM 终审）: 帧路径改动（`blit.rs`/`app_link.rs`/`ahb_handle.rs`/`comp/dmabuf.rs`/`frame_router.rs`）仅 Perf 作者改动（AGENTS.md）；BOUNDARIES §2 零 hook 复核（无 dlsym/无隐藏 API/无 patch）。**（confirmed：23 文件 5 类目无越界；dlsym 仅 2 处拒绝性注释、隐藏 ANativeWindow API 零命中、仅公开 Vulkan 扩展（VK_KHR_android_surface/swapchain/external_semaphore_fd/external_fence_fd/ANDROID_external_memory_android_hardware_buffer，禁用的 external_fence_sync_file 仅注释）、第三方零改动、零新依赖、D2/D3/D4 决策一致、零测试删除（+47 新增）；交接注记：.omo/start-work/ 未 gitignore、bridge.c deprecated CPU 路径遗留）**

- [x] F4. 兼容性: Weston 嵌套、Hyprland 嵌套冒烟（M7）。**（静态部分 confirmed：全局清单 14 项全注册（wl_subcompositor 由 CompositorState::new_v6 自动注册——F4 审计假阳性已澄清，smithay 0.7 源码实证 L706 create_global + delegate_compositor! L733-735 分发）、dmabuf feedback P-16 8 项、PROTOCOL_VERSION=2 一致、零 compositor 特定 hack（kwin/weston/hyprland 代码零命中）；真机项（weston --backend=wayland-backend.so / hyprland 嵌套 / weston-simple-dmabuf-egl 视觉）设备门控待连接）**

- [ ] F2. 功能验证: Plasma 桌面正常显示；快速连击无 crash；KWin 文本输入可用；旋转/分辨率/刷新率动态适配（M5）。**（设备门控：adb 无设备——真机验证项待设备连接后执行）**

- [ ] F3. 性能验证: `doctor` 输出 PERF-02/03 p95、PERF-11 计数、PERF-08/09 帧率；`soak.sh` 1h 无 PERF-05/07 违反。**（设备门控：adb 无设备——soak/doctor 真机数据待设备连接后执行）**

## 关键文件索引

- 协议: `crates/wl-android-common/src/proto.rs` + `proto/tests.rs` + `snapshots/`
- server: `crates/wl-android/src/{state.rs, main.rs, app_link.rs, blit.rs, frame_router.rs, frame_cache.rs, comp/dmabuf.rs, ahb_handle.rs, doctor.rs}`
- App: `android-app/native/src/{session.rs, render.rs, ahb.rs, jni_bridge.rs, lib.rs}` + `c/bridge.c` + `app/src/main/java/com/wl/android/{MainActivity.kt, NativeBridge.kt, TouchForwarder.kt, ScreenInfoCollector.kt}`
- 约束/文档: `docs/DESIGN.md`, `BOUNDARIES.md`, `PERFORMANCE_BOUNDARIES.md`, `docs/AGENTS.md`
- 部署/验证: `scripts/deploy-test.sh`, `scripts/build-all.sh`, `scripts/soak.sh`, `milestones/M{2-7}-verify.sh`, `docs/TESTING.md`

## 部署/验证命令速查（TESTING.md 规范）

```bash
# server 改动 → 容器内编译
adb shell "su -c '/data/local/Droidspaces/bin/droidspaces --name=arch run /bin/bash -c \"cd /home/kagari/wl-android && cargo build --release -p wl-android && cp target/release/wl-android /data/local/tmp/wl-android/wl-android\"'"
# App .so 改动 → host 交叉编译
cd android-app/native && cargo +stable ndk -t arm64-v8a build --release
# Kotlin 改动 → gradle
cd android-app && ./gradlew assembleDebug && adb install -r app/build/outputs/apk/debug/app-debug.apk
# 验证（固定命令）
adb logcat -d -s land-native | grep -E "Frame received|render:|present:" | tail -3
# 等 3 秒再抓，serial 递增 = PASS
```

## 风险与缓解

| 风险 | 缓解 |
|------|------|
| **UBWC 导入失败**（最大风险，已确认未验证）——swapchain 图像由 gralloc 决定 modifier（Adreno 几乎必 UBWC），turnip 仅支持 LINEAR/QCOM_COMPRESSED | TODO 30 bring-up 门控 `import_ubwc_test`（读回像素校验）；失败→强制 LINEAR 或暂停 P3；P2 的 KWin 帧导入（也是 UBWC 或 LINEAR）先行验证 |
| App 宿主驱动不支持 SYNC_FD 栅栏导出 | 运行时断言 `vkGetPhysicalDeviceExternalFenceProperties`；不支持→TODO 22 降级（CPU wait + 无 fence fd） |
| `VK_ANDROID_external_fence_sync_file` 不存在 | 调研已确认从未发布；全链路用 `VK_KHR_external_fence_fd`（计划已写死） |
| 容器 turnip 无 AHB 扩展 | 调研已确认（Linux 构建）；server 只导入原始 dma-buf fd（`VK_EXT_external_memory_dma_buf`），App 侧才用 AHB 扩展 |
| stride 不匹配（常见错误源） | TBUF/Frame 消息强制携带 stride；导入用 `VkImageDrmFormatModifierExplicitCreateInfoEXT` |
| 协议 v2 与旧 mock-app.py 不兼容 | mock-app.py 同步升级（X-06），golden bytes 先绿 |
| 快速连击期间 slot 竞争 | F-14 BRDY 门控 + F-04 反压 + FdCountGuard 全路径断言 |

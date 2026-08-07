# PERFORMANCE_BOUNDARIES.md — 性能约束（硬性）

> 规则编号 `PERF-xx`。每条约束都必须绑定"测量方法"——不可测量的约束不成立。
> 目标设备：一加平板 3（Snapdragon 8 Elite / Adreno 830，3392×2400 @ 144Hz LTPO，
> LPDDR5X ≈ 76.8 GB/s）。

> **SUPERSEDED（模型部分）** — §2/§3 的带宽与延迟预算基于已删除的 blit + swapchain
> 路径（commit b3491f4，v2 主路径）。当前 v3 主路径为 **dmabuf 零拷贝**：KWin GPU
> 渲染 dmabuf → server commit 提取 → fd dup 零拷贝转发（无像素拷贝）→ App mmap +
> `ANativeWindow_lock` CPU 呈现。本文件的 PERF 编号与阈值表结构保留；blit 相关行已
> 标注，**需按 dmabuf 路径重新校准**。

## 1. 硬性约束表

| 编号 | 约束 | 阈值 | 测量方法 |
|------|------|------|----------|
| PERF-01 | 零 CPU 像素拷贝 | 帧路径上 CPU **不得触碰像素**（禁止对帧 dmabuf `mmap`/`memcpy`） | 代码审查门禁 + `strace` 抽查无帧 fd 的 mmap；`perf top` 无 memcpy 热点 |
| PERF-02 | 端到端帧延迟 @60Hz | < 16.7 ms | serial 打点：commit 时刻(服务端) ↔ present 完成(App)，doctor 汇总 p95 |
| PERF-03 | 端到端帧延迟 @144Hz | < 6.9 ms | 同上 |
| PERF-04 | 触摸端到端延迟（注入侧） | TOUC.time_ms → wl_touch 注入 < 10 ms | 两端时戳差值统计（CLOCK_MONOTONIC 换算），doctor 汇总 p95 |
| PERF-05 | wl-android 常驻内存 | RSS < 32 MB | `/proc/<pid>/status` VmRSS，1h 采样 |
| PERF-06 | App 常驻内存 | < 128 MB（Java+native PSS） | `dumpsys meminfo` |
| PERF-07 | fd 泄漏 | 连续运行 1h，`/proc/<pid>/fd` 计数波动 ≤ 稳态在途上限 | 两端各 60s 采样脚本；CI 层由 FdCountGuard（X-04）覆盖逻辑路径 |
| PERF-08 | 帧率（桌面静止） | ≥ 60 FPS | App present 计数器（doctor 页显示） |
| PERF-09 | 帧率（窗口拖动） | ≥ 144 FPS（144Hz 模式） | 同上 + KWin fps 显示交叉验证 |
| PERF-10 | 容器侧 CPU | < 20%（单核，稳态 144Hz） | `top -p` 采样 |
| PERF-11 | 稳态零导入 | 稳态下 App 每帧 Vulkan 导入次数 = 0（buffer_id 缓存命中） | 导入计数器：warmup 后增量为 0 |

> 注：PERF-01/02/03/11 的测量方法基于 v2 语义（serial 打点 / Vulkan 导入计数 / 零 CPU
> 像素拷贝）。当前 v3 无状态协议已无 serial、无 Vulkan 导入（App 侧为 mmap +
> `ANativeWindow_lock` CPU 拷贝）——上述测量方法需按 dmabuf 路径重新校准，阈值保留。

## 2. 带宽预算（基于已删 blit + swapchain 主路径；需按 dmabuf 零拷贝路径重新校准）

单帧 3392×2400×4 B ≈ 31.1 MiB（linear）。UBWC 压缩典型压缩比 ~50%，
实际传输量约 15.5 MiB per blit pass。

| 路径 | 组成 | @144Hz 合计 (linear) | @144Hz 合计 (UBWC ~50%) | 占 76.8 GB/s |
|------|------|----------------------|------------------------|--------------|
| KWin 渲染写出 | 4.7 GB/s | — | — | — |
| direct（已删除，830 不可用） | KWin 写 + App 采样读 + swapchain 写 | ≈ 14.1 GB/s | N/A（direct 不可用） | — |
| **blit**（v2 主路径，**已删除** commit b3491f4） | KWin 渲染写 + blit 读 + blit 写（swapchain 图像即 AHB slot，present 零拷贝） | ≈ 14.1 GB/s | **≈ 7.0 GB/s** | **≈ 9%** |

> 注：本表为 blit 时代模型（已删除），仅作历史基准。当前 dmabuf 零拷贝主路径无
> server 侧 blit pass、无 UBWC 压缩参与，带宽构成不同——需按当前实现重新测量后更新本表。

结论（**基于已删 blit 路径**）：blit 路径 UBWC 启用后带宽占用 ≈ 9%，充裕。swapchain
呈现零额外拷贝（present 即移交，App 不采样像素），带宽由 ADR #15 启用的 UBWC 压缩吸收。
当前 dmabuf 零拷贝路径无 server 侧 blit/swapchain，带宽构成不同，需按当前实现重新校准。

## 3. 延迟预算分解（144Hz，帧预算 6.94 ms）

| 段 | 预算 | 备注 |
|----|------|------|
| commit → sendmsg | < 0.5 ms | state.rs commit 提取 + 一次 syscall（fd dup 转发） |
| socket 传输（16 B + fd） | < 0.2 ms | SOCK_STREAM 本机（u32 长度前缀） |
| App 收帧 → 呈现 | < 0.7 ms | mmap + ANativeWindow_lock CPU 拷贝（无 server 侧像素拷贝；PERF-01 需按此重新校准） |
| GPU blit + present 排队（blit 路径，已删除） | — | — |
| 余量（调度抖动） | ≈ 3 ms | |

（blit 模式已删除，commit b3491f4——无额外 GPU blit pass。）当前为单一路径，doctor
按 dmabuf 零拷贝路径报告 PERF-02/03；本段预算基于 blit 路径，需重新校准。

## 4. 设计层面的性能规则

- **PERF-12** 在途窗口 `MAX_IN_FLIGHT = 2`（F-04）：延迟与吞吐的折中定点；改动
  需重新跑 PERF-02/03 全量验证。
- **PERF-13** 帧消息固定 16 B + 1 fd、无堆分配编解码（zerocopy 视图）；触摸消息路径
  （TouchForwarder → ring buffer → sendmsg）禁止分配与锁。
- **PERF-14**（blit 路径，已删除）blit 使用独立 transfer queue（若 turnip 暴露），避免与合成器渲染
  争抢 graphics queue。—— 需按 dmabuf 路径重新校准（当前 server 侧无 GPU 工作）。
- **PERF-15**（swapchain 路径，已删除）App 侧 swapchain：优先 `MAILBOX`，不可用则 `FIFO`；144Hz 经
  `preferredDisplayModeId` + `Surface.setFrameRate` 显式请求。—— 当前 App 用
  `ANativeWindow_lock` CPU 呈现，144Hz 经 `ScreenInfoCollector` 的 display mode 上报 CONF，
  需按当前路径重新校准。

## 5. 验收程序

1. **CI（每次合入）**：X-03/X-04 全绿；帧路径单元测试（`state.rs`/`app_link.rs`/`transport.rs`）无回归
   超过 10%。
2. **真机（里程碑关卡）**：doctor 输出 PERF-02/03/04 p95、PERF-11 计数；
   `scripts/soak.sh` 跑 1h 输出 PERF-05/06/07/10 采样曲线。
3. 任一硬性约束不达标 → 里程碑不关闭。

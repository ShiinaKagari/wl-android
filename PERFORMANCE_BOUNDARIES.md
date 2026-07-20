# PERFORMANCE_BOUNDARIES.md — 性能约束（硬性）

> 规则编号 `PERF-xx`。每条约束都必须绑定"测量方法"——不可测量的约束不成立。
> 目标设备：一加平板 3（Snapdragon 8 Elite / Adreno 830，3392×2400 @ 144Hz LTPO，
> LPDDR5X ≈ 76.8 GB/s）。

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

## 2. 带宽预算（v1 强制 linear 的代价核算）

单帧 3392×2400×4 B ≈ 31.1 MiB（linear）。UBWC 压缩典型压缩比 ~50%，
实际传输量约 15.5 MiB per blit pass。

| 路径 | 组成 | @144Hz 合计 (linear) | @144Hz 合计 (UBWC ~50%) | 占 76.8 GB/s |
|------|------|----------------------|------------------------|--------------|
| KWin 渲染写出 | 4.7 GB/s | — | — | — |
| direct | KWin 写 + App 采样读 + swapchain 写 | ≈ 14.1 GB/s | N/A（direct 不可用） | — |
| **blit** (v1) | direct + blit 读写各一次 | ≈ 23.5 GB/s | **≈ 11.7 GB/s** | **≈ 15%** |

结论：blit 路径 UBWC 启用后带宽占用 ≤ 15%，充裕。UBWC 已是 v1 特性（非 v2），
见 DESIGN.md ADR #15。

## 3. 延迟预算分解（144Hz，帧预算 6.94 ms）

| 段 | 预算 | 备注 |
|----|------|------|
| commit → sendmsg | < 0.5 ms | frame_router 纯逻辑 + 一次 syscall |
| socket 传输（80 B + fd） | < 0.2 ms | SEQPACKET 本机 |
| App 收帧 → 提交 GPU | < 0.7 ms | 稳态零导入（PERF-11），仅录制 blit cmd |
| GPU blit + present 排队 | < 2.5 ms | 与合成器 GPU 工作并行 |
| 余量（调度抖动） | ≈ 3 ms | |

blit 模式额外 +1 次 GPU blit（容器侧），预算内但侵蚀余量——doctor 必须能区分
两种模式分别报告 PERF-02/03。

## 4. 设计层面的性能规则

- **PERF-12** 在途窗口 `MAX_IN_FLIGHT = 2`（F-04）：延迟与吞吐的折中定点；改动
  需重新跑 PERF-02/03 全量验证。
- **PERF-13** 帧消息固定 80 B、无堆分配编解码（zerocopy 视图）；触摸消息路径
  （TouchForwarder → ring buffer → sendmsg）禁止分配与锁。
- **PERF-14** blit 使用独立 transfer queue（若 turnip 暴露），避免与合成器渲染
  争抢 graphics queue。
- **PERF-15** App 侧 swapchain：优先 `MAILBOX`，不可用则 `FIFO`；144Hz 经
  `preferredDisplayModeId` + `Surface.setFrameRate` 显式请求。

## 5. 验收程序

1. **CI（每次合入）**：X-03/X-04 全绿；`frame_router` 基准测试（criterion）无回归
   超过 10%。
2. **真机（里程碑关卡）**：doctor 输出 PERF-02/03/04 p95、PERF-11 计数；
   `scripts/soak.sh` 跑 1h 输出 PERF-05/06/07/10 采样曲线。
3. 任一硬性约束不达标 → 里程碑不关闭。

# PERFORMANCE BOUNDARIES — wl-android 性能约束

## 硬性限制

| 指标 | 限制 | 测量方法 | 违反后果 |
|------|------|----------|----------|
| CPU 触碰像素 | **禁止** | 代码审查 + `perf stat` dTLB-loads | 架构违规，拒绝合并 |
| surface commit → socket 发送 | **< 500µs** | `std::time::Instant::now()` | 帧延迟堆积 |
| 端到端帧延迟 (60Hz) | **< 16.7ms** | 合成器 commit → `vkQueuePresentKHR` | 丢帧 |
| 端到端帧延迟 (144Hz) | **< 6.9ms** | 同上 | 丢帧 |
| 触摸端到端延迟 | **< 10ms** | MotionEvent → `wl_touch.down` 回调 | 触摸不跟手 |
| land (插件) 内存 | **< 4MB** | `/proc/self/status VmRSS` | 容器 OOM |
| landd (守护) 内存 | **< 2MB** | 同上 | 宿主 OOM |
| land-app (App) 内存 | **< 64MB** | `ActivityManager.getMemoryClass()` | App 被 kill |
| fd 泄漏 | **零容忍** | CI 中 `/proc/self/fd` 快照对比 | 合成器崩溃 |

## 内存分配明细

### land (< 4MB)
- 代码段 + BSS: ~500KB
- Socket 缓冲区: 64KB
- 并发状态: ~200KB
- 无帧缓冲（零拷贝）

### landd (< 2MB)
- 二进制: ~800KB
- epoll 事件数组: 16KB
- 客户端状态 (MAX 16): ~256KB
- I/O 缓冲区: 256KB

### land-app (< 64MB)
- Vulkan 设备内存:
  - 4K 帧 (3840×2160×4): ~32MB/帧
  - 双缓冲: 64MB
- Vulkan 实例 + 设备: ~8MB
- Kotlin 运行时: ~8MB
- 其他: ~16MB 余量
- **1080p 分辨率时自动降到单缓冲，峰值 < 32MB**

## 基准测试

每次 CI 运行：
```
cargo bench -p land
cargo bench -p landd
```

关键基准：
- `bench_send_frame`: socket SCM_RIGHTS 发送延迟（< 500µs）
- `bench_forward_frame`: landd 转发延迟（< 100µs）
- `bench_recv_fds`: SCM_RIGHTS fd 接收延迟

## 退化检测

- 基准测试比较当前结果与 baseline（存储在 `crates/*/benches/.baseline`）
- 退化 >10% → CI 告警（非阻塞）
- 退化 >30% → CI 失败

## fd 泄漏检测

每次 CI 测试运行：
```bash
# 测试前后对比 fd 计数
BEFORE=$(ls /proc/self/fd | wc -l)
# ... 运行测试 ...
AFTER=$(ls /proc/self/fd | wc -l)
if [ "$BEFORE" != "$AFTER" ]; then exit 1; fi
```

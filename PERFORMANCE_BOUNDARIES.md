# PERFORMANCE BOUNDARIES — wl-android 性能约束

## 硬性限制

| 指标 | 限制 | 测量方法 | 违反后果 |
|------|------|----------|----------|
| CPU 触碰像素 | **禁止** | 代码审查 | 架构违规 |
| surface commit → socket 发送 | **< 500µs** | `clock_gettime` 埋点 | 帧延迟堆积 |
| 端到端帧延迟 (60Hz) | **< 16.7ms** | compositor commit → `vkQueuePresentKHR` | 丢帧 |
| 端到端帧延迟 (144Hz) | **< 6.9ms** | 同上 | 丢帧 |
| socketd 内存 | **< 2MB** | `VmRSS` | 宿主 OOM |
| land-app (App) 内存 | **< 64MB** | `ActivityManager.getMemoryClass()` | App 被 kill |
| fd 泄漏 | **零容忍** | CI `/proc/self/fd` 前后对比 | 崩溃 |

## 内存分配明细

### socketd (< 2MB)
- 二进制: ~6KB (动态链接)
- poll 数组 + 缓冲区: 8MB (4MB × 2)
- 无帧缓冲

### land-app (< 64MB)
- Vulkan 设备内存:
  - 4K 帧 (3840×2160×4): ~32MB/帧
  - 双缓冲: 64MB
- Vulkan 实例 + 设备: ~8MB
- Kotlin 运行时: ~8MB
- 其他: ~16MB 余量
- **1080p 分辨率时自动降到单缓冲，峰值 < 32MB**

## fd 泄漏检测

每次测试后对比 fd 计数：

```bash
before=$(ls /proc/self/fd | wc -l)
# 运行测试
after=$(ls /proc/self/fd | wc -l)
[ "$before" = "$after" ] || exit 1
```

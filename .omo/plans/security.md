# wl-android 安全开发规范

## 1. 最小权限原则

### 1.1 进程身份

| 组件 | 运行身份 | 理由 |
|------|---------|------|
| wl-android compositor | 非特权用户（非 root） | Wayland 禁止 root 跑 GUI；compositor 以启动它的用户身份提供 socket |
| KWin / weston 客户端 | 与 compositor 同用户 | socket 权限自然满足 |
| Android App | Android 沙箱 | 标准隔离 |

**禁止**：生产路径以 root 运行 compositor 或 GUI 客户端。

### 1.2 socket 权限

```
正确：owner 读写，同组可连
srwxrwx--- $USER $USER $XDG_RUNTIME_DIR/wl-android/land-0

禁止：666 开放给所有人，或 root-only
```

- Wayland socket：`chmod 660`，owner 为 compositor 进程用户
- Land socket：`chmod 660`，host App 通过 bind mount 访问

### 1.3 文件系统

| 路径 | 用途 | 权限 |
|------|------|------|
| `$XDG_RUNTIME_DIR/wl-android/land-0` | Wayland socket | 660, $USER:$USER |
| `$XDG_RUNTIME_DIR/wl-android/land.sock` | App socket | 660, $USER:$USER |
| `$HOME/.local/share/wl-android/` | 日志/pid | 700, $USER:$USER |

**禁止**：socket 放 `/tmp/`、bind mount 整个目录（只 bind 需要的文件）

## 2. 内存与 IPC 安全

### 2.1 unsafe 代码

所有 `unsafe` 块必须：
1. 单行 SAFETY 注释声明安全不变量
2. unsafe 块最小化，safe 代码移出
3. 提交信息标注 `unsafe:` 前缀

### 2.2 fd 生命周期

```
规则：谁创建谁关闭。发送方 dup 后发送副本，接收方用完关闭。

正确：
  Server: fd = memfd_create()  →  sendmsg(dup_fd)  →  保留 fd 复用
  App:    recvmsg()  →  use(fd)  →  close(fd)

错误：
  Server: fd = memfd_create()  →  sendmsg(fd)  →  fd 已消费，后续 UB
```

### 2.3 memfd 规范

- 创建后 fcntl(F_ADD_SEALS, F_SEAL_SEAL|F_SEAL_SHRINK|F_SEAL_GROW|F_SEAL_WRITE)
- 复用（双缓冲）时不加 F_SEAL_WRITE，需写入前后加同步
- 大小固定 `width * height * 4`

### 2.4 mmap 安全

- 必须检查 MAP_FAILED
- 使用后必须 munmap
- mmap 引用的 &[u8] 不可跨线程共享

## 3. 协议安全

### 3.1 帧协议

| 威胁 | 缓解 |
|------|------|
| 帧数据篡改 | memfd + F_SEAL_WRITE |
| 缓冲区溢出 | FrameMessage 固定大小，decode 校验 |
| fd 泄漏 | 新 fd 到达时 close 旧 fd |

### 3.2 Wayland 协议

- 接受同用户连接（SO_PEERCRED 校验 uid）
- 协议扩展白名单制

## 4. GPU 访问

- 用户必须属于 `/dev/kgsl-3d0`（或等效 GPU 设备节点）对应的设备组
- 容器运行时通过设备 cgroup 或 bind mount 暴露 GPU 节点时，必须同步映射组权限
- VK_ICD_FILENAMES 显式指定 ICD，不信任自动发现
- 渲染后端：turnip 优先，llvmpipe fallback

## 5. 构建与检查

- `cargo audit` PR 必须通过
- `cargo clippy -- -D warnings` 零容忍
- 新增依赖评估许可证、维护状态、unsafe 量

## 6. PR 检查清单

- [ ] 无新增 unsafe 块，或每个有 SAFETY 注释
- [ ] fd 创建/关闭成对
- [ ] socket 权限正确（非 777/666，非 root-only）
- [ ] 无硬编码路径
- [ ] cargo test + clippy 通过
- [ ] 无 root 权限依赖

## 7. 敏感操作审计点

以下操作必须在审查中标注：
- unsafe 块
- mmap / munmap
- sendmsg / recvmsg with SCM_RIGHTS
- memfd_create / dup / close
- chmod / chown
- 环境变量注入点（VK_ICD_FILENAMES 等）

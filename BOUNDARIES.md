# BOUNDARIES — wl-android 边界约束（不可协商）

## 技术边界

### ❌ 以下行为禁止
1. **修改第三方代码**：不修改合成器（Weston/KWin/Hyprland）、内核、Mesa、Wayland、wlroots 的任何一行源码
2. **OpenGL ES**：仅使用 Vulkan（VK_KHR_external_memory_fd），无 GLES fallback
3. **自定义 Wayland 协议扩展**：使用标准后端插件机制，不定义新协议
4. **危险 API**：
   - 禁止 `system()`、`popen()`、`exec()`、`dlopen()`
   - 禁止不安全的反序列化（如 `serde` JSON/XML）

### ✅ 以下必须
1. **零拷贝**：所有路径禁止 CPU 触碰像素数据（DMA-BUF fd 传递）
2. **SAFETY 注释**：所有 `unsafe` 块必须有 `// SAFETY:` 注释说明前提条件
3. **RAII fd 管理**：必须使用 `std::os::unix::io::OwnedFd` 或等价 RAII 包装管理 fd
4. **Socket 位置**：`/data/local/tmp/land.sock`（tmpfs），权限 0666，可通过 `LAND_SOCKET` 环境变量覆盖
5. **fd dup**：跨进程传递的 DMA-BUF fd 必须 `dup()` 后使用

## 安全边界

1. App 不申请 root 权限
2. 守护进程（landd）通过 Magisk 以 root 运行
3. Socket 权限 0666（容器和 App 可访问）
4. 所有 socket 通信使用二进制协议（非文本），防注入

## 架构边界

1. 容器侧唯一组件：`wl-android-compositor`（嵌套 wlroots 合成器）
2. Android 侧：socketd（Magisk 保活）+ land-app（APK）
3. 容器 ↔ 宿主机通信仅通过 bind mount 的 socket 文件
4. 不替换系统库，不修改系统配置文件
5. socketd 常驻内存，管理 socket 生命周期

## 部署边界

| 目标 | 方式 | 备注 |
|------|------|------|
| land (容器) | `.deb` 包 或 手动复制 `.so` | 安装到 `/usr/lib/wlroots/` |
| landd (宿主) | Magisk 模块 ZIP | 刷入后开机自启 |
| land-app (宿主) | APK | 普通安装 |

## 容器修改清单（允许的）

1. 在容器内创建 `/run/` 目录，通过 `-B /data/local/tmp/land.sock:/run/land.sock` bind mount
2. 安装 land `.deb` 包
3. 设置 `LAND_SOCKET` 环境变量（可选，默认值自动生效）
4. 加载 `libland_wlroots.so` 作为 wlroots 渲染器

# wl-android 收尾开发计划

## TL;DR

修复 App .so 编译错误 + 容器内重建部署 server + 端到端验证画面。

## TODOs

- [x] 1. 修复 App .so 编译 — `session.rs` `recv_raw_with_fds` E0502 borrow
    - 文件: `android-app/native/src/session.rs`
    - 改动: 用 `{}` block 限定 `iov` 生命周期，删除 176-189 行重复 fds 提取代码
    - 验证: `cargo +stable ndk -t arm64-v8a build --release` 通过
    - 部署: `adb push` .so 到 App 安装目录

- [x] 2. 容器内重建 + 部署 server
    - root: `chown -R kagari:kagari /tmp/wl-android /run/wl-android`
    - kagari: `cd /tmp/wl-android && git pull && cargo build --release -p wl-android`
    - `cp target/release/wl-android /run/wl-android/wl-android`
    - 验证: memfd_create 在 symbol table，disasm 确认调用存在

- [x] 3. 端到端验证 weston 画面到达 App
    - 启动 server + weston-simple-shm + App
    - 验证: `adb logcat | grep FRAME` 显示 `data=32563200B` ✅
    - 帧率: ~23 FPS, serial 持续递增

## Final verification wave

- [x] F1. 代码审查: session.rs 变更正确性
- [x] F2. 功能验证: App 画面正常显示 ✅
    - 屏幕显示 weston-simple-shm 彩色动画
    - 内容铺满 3340x2360（近全屏）
    - 两截图间 48% 像素变化（动画运行中）
    - 无 crash、无乱码

#!/usr/bin/env bash
# 在容器内运行：构建 wl-android 容器侧组件
set -euo pipefail

echo "=== wl-android container-side build ==="
echo "Target: $(uname -m) / $(uname -s)"

# 1. 依赖
if command -v pacman &>/dev/null; then
    pacman -Syu --noconfirm base-devel git rust cargo wlroots wayland-server libdrm
elif command -v apt &>/dev/null; then
    apt update && apt install -y build-essential git rustc cargo libwlroots-dev libwayland-dev libdrm-dev
fi

# 2. 克隆代码
if [ ! -d wl-android ]; then
    git clone https://github.com/ShiinaKagari/wl-android
fi
cd wl-android

# 3. 构建嵌套合成器 (唯一容器侧组件)
echo "Building wl-android-compositor..."
cd test-compositor && make && sudo cp wl-android-compositor /usr/local/bin/ && cd ..

# 4. 验证
echo "=== Build complete ==="
file /usr/local/bin/wl-android-compositor

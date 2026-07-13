#!/usr/bin/env bash
# 在 8 Elite Droidspaces 容器内运行：构建 wl-android 容器侧组件
set -euo pipefail

echo "=== wl-android container-side build ==="
echo "Target: $(uname -m) / $(uname -s)"

# 1. 依赖
if command -v pacman &>/dev/null; then
    pacman -Syu --noconfirm base-devel git rust cargo wlroots wayland-server libdrm
elif command -v apt &>/dev/null; then
    apt update && apt install -y build-essential git rustc cargo libwlroots-dev libwayland-dev libdrm-dev
fi

# 2. 克隆仓库
if [ ! -d wl-android ]; then
    git clone https://github.com/ShiinaKagari/wl-android
fi
cd wl-android

# 3. 构建 libland_wlroots.so (wlroots 后端插件)
echo "Building libland_wlroots.so..."
cargo build --target aarch64-unknown-linux-gnu --release -p land -p land-common
sudo cp target/aarch64-unknown-linux-gnu/release/libland_wlroots.so /usr/lib/wlroots/

# 4. 构建 landd (可在容器内调试运行)
echo "Building landd..."
cargo build --target aarch64-unknown-linux-gnu --release -p landd
sudo cp target/aarch64-unknown-linux-gnu/release/landd /usr/local/bin/

# 5. 构建嵌套合成器
echo "Building wl-android-compositor..."
cd test-compositor && make && sudo cp wl-android-compositor /usr/local/bin/ && cd ..

# 6. 验证
echo "=== Build complete ==="
file target/aarch64-unknown-linux-gnu/release/libland_wlroots.so
file target/aarch64-unknown-linux-gnu/release/landd
file /usr/local/bin/wl-android-compositor

cat << 'EOF'

Usage:
  export LAND_SOCKET=/dev/socket/land.sock
  wl-android-compositor &
  WAYLAND_DISPLAY=wl-android-0 gnome-shell --nested &
EOF

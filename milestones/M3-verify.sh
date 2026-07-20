#!/bin/bash
# M3-verify.sh — App 渲染真机验收
# 前置条件：
#   - M2 通过
#   - wl-android-app APK 已安装
#   - 容器侧 wl-android 已启动
# 运行位置：开发机（通过 adb）或直接在平板端操作
set -euo pipefail

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
pass() { echo -e "  ${GREEN}✅${NC} $*"; }
fail() { echo -e "  ${RED}❌${NC} $*"; exit 1; }

echo "=== M3 Verification: App Rendering ==="
echo

# V-05: App connects to land.sock
echo "--- V-05: App socket connection ---"
echo "  1. Open wl-android-app on tablet"
echo "  2. Check App debug page shows 'State: Active'"
echo "  3. Confirm App debug page shows 'protocol_version=1, mode=blit'"
echo
read -rp "  Did the App show Active state? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-05: App not connected"; fi
pass "V-05: App connected"

# V-06: slot registration (blit mode)
echo
echo "--- V-06: Slot registration (blit mode) ---"
echo "  1. On App debug page, check 'Slots: 3/3 registered'"
echo "  2. Container log shows 3 SlotBuffer messages received"
echo
read -rp "  Are all 3 slots registered? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-06: Slots not all registered"; fi
pass "V-06: 3/3 slots registered"

# V-07: Frame receiving + ack
echo
echo "--- V-07: Frame receive → ack → release cycle ---"
echo "  Check App debug page:"
echo "    - Frame counter increments (serial N, N+1, ...)"
echo "    - Ack counter matches frame counter"
echo "    - No buffer freeze (App debug shows 'release count' matches)"
echo "    - Container log shows no 'buffer pool exhausted' warnings"
echo
read -rp "  Frame cycle stable? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-07: Frame cycle broken"; fi
pass "V-07: Frame → ack → release cycle stable"

# V-08: Visual rendering
echo
echo "--- V-08: Visual output ---"
echo "  1. Run weston-simple-dmabuf-egl in container:"
echo "     WAYLAND_DISPLAY=land-0 weston-simple-dmabuf-egl"
echo "  2. Verify the test pattern appears on tablet screen"
echo "  3. Colors are correct (not garbled / wrong modifier)"
echo
read -rp "  Test pattern visible + correct colors? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-08: Visual rendering incorrect"; fi
pass "V-08: Visual output correct"

echo
echo "=== M3: ALL CHECKS PASSED ==="

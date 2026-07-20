#!/bin/bash
# M6-verify.sh — KWin/Plasma 拉起真机验收
# 前置条件：
#   - M5 通过（动态配置正常）
#   - 容器内已安装 KDE Plasma（startplasma-wayland 可用）
# 运行位置：开发机（adb） + 平板端 + 容器内操作
set -euo pipefail

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
pass() { echo -e "  ${GREEN}✅${NC} $*"; }
fail() { echo -e "  ${RED}❌${NC} $*"; exit 1; }
warn() { echo -e "  ${YELLOW}⚠️${NC} $*"; }

echo "=== M6 Verification: KWin/Plasma ==="
echo

# V-18: KWin connects to land-0
echo "--- V-18: KWin nested compositor connects ---"
echo "  1. Container: wl-android is running (from M2)"
echo "  2. Container: WAYLAND_DISPLAY=land-0 kwin_wayland &"
echo "     (or: startplasma-wayland)"
echo "  3. Container log shows: 'xdg_toplevel created'"
echo "  4. Container log shows: 'zwp_linux_dmabuf_v1 client bound'"
echo "  5. No protocol error messages in container log"
echo
read -rp "  KWin connected without protocol errors? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-18: KWin connection failed"; fi
pass "V-18: KWin connected"

# V-19: Plasma desktop visible
echo
echo "--- V-19: Plasma desktop visible on tablet ---"
echo "  1. App shows frame content (not black/static)"
echo "  2. Plasma panel (taskbar) visible at bottom"
echo "  3. Wallpaper rendered"
echo "  4. Frame rate stable (App debug shows consistent serial increments)"
echo
read -rp "  Plasma desktop visible + running? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-19: Plasma not visible"; fi
pass "V-19: Plasma visible"

# V-20: Touch interaction
echo
echo "--- V-20: Plasma touch interaction ---"
echo "  1. Tap KDE application launcher → menu opens"
echo "  2. Drag application window → window moves"
echo "  3. Tap window close button → window closes"
echo "  4. Pinch gestures (if supported) → not crash"
echo
read -rp "  Plasma touch interaction works? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-20: Touch interaction broken"; fi
pass "V-20: Touch interaction OK"

# V-21: Application windows
echo
echo "--- V-21: Application windows ---"
echo "  1. Launch a GUI app (e.g. konsole, kwrite)"
echo "  2. App window appears and is interactive"
echo "  3. Window resize / maximize works"
echo "  4. Window content renders correctly (no corruption)"
echo
read -rp "  GUI applications work? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-21: Applications broken"; fi
pass "V-21: Applications OK"

# V-22: Rotation + Plasma
echo
echo "--- V-22: Rotation while Plasma running ---"
echo "  1. Rotate tablet 90°"
echo "  2. Plasma panel repositions (should move to new bottom edge)"
echo "  3. Desktop wallpaper reflows"
echo "  4. Application windows reposition correctly"
echo "  5. No crash or protocol errors"
echo
read -rp "  Rotation with Plasma running? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-22: Rotation + Plasma broken"; fi
pass "V-22: Rotation + Plasma OK"

# V-23: Missing protocol tolerance
echo
echo "--- V-23: Missing protocol check ---"
echo "  Check container log for any 'unknown global' or 'unsupported protocol' warnings."
echo "  If KWin requires a protocol we don't implement, it should appear here."
echo
echo "  ℹ️  Run: grep -i 'unknown\|unsupported\|protocol error' <container-log>"
echo
read -rp "  No unknown protocol errors? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-23: Missing protocol detected"; fi
pass "V-23: No missing protocols"

echo
echo "=== M6: ALL CHECKS PASSED ==="

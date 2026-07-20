#!/bin/bash
# M5-verify.sh — Dynamic configuration verification
# 前置条件：
#   - M4 通过（触摸正常）
#   - 容器内 weston-info / weston-simple-dmabuf-egl 可用
# 运行位置：开发机（adb） + 平板端操作 + 容器内操作
set -euo pipefail

RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'
pass() { echo -e "  ${GREEN}✅${NC} $*"; }
fail() { echo -e "  ${RED}❌${NC} $*"; exit 1; }

echo "=== M5 Verification: Dynamic Configuration ==="
echo

# V-14: Screen rotation
echo "--- V-14: Screen rotation → wl_output update ---"
echo "  1. Rotate tablet 90° (auto-rotate enabled)"
echo "  2. App debug page: new ConfigMessage sent with swapped w/h"
echo "  3. Container log: 'wl_output mode updated to W×H'"
echo "  4. In container: WAYLAND_DISPLAY=land-0 weston-info | grep physical"
echo "     → shows new dimensions matching rotated orientation"
echo "  5. Desktop content reflows to new aspect ratio"
echo
read -rp "  Rotation → desktop reflow? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-14: Rotation not handled"; fi
pass "V-14: Rotation OK"

# V-15: Resolution change
echo
echo "--- V-15: Resolution change (adb shell wm size) ---"
echo "  1. On host: adb shell wm size 1600x1200"
echo "  2. App debug page: new ConfigMessage with new w/h"
echo "  3. Container log: wl_output + xdg_toplevel configure with new size"
echo "  4. Desktop reflows; App surface shows letterboxing correctly"
echo "  5. Restore: adb shell wm size reset"
echo "  6. Desktop returns to native resolution"
echo
read -rp "  Resolution change → adapt + restore? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-15: Resolution change not handled"; fi
pass "V-15: Resolution change OK"

# V-16: Refresh rate change
echo
echo "--- V-16: Refresh rate change ---"
echo "  1. If device supports display mode switching:"
echo "     Settings → Display → Screen refresh rate → 60Hz"
echo "  2. App debug page: new ConfigMessage with refresh=60000"
echo "  3. Container log: 'wl_output mode updated, refresh=60000'"
echo "  4. Frame callback cadence adjusts to 60Hz"
echo "  5. Restore to 144Hz → App debug shows 144000, cadence resumes"
echo "  ℹ️  If device lacks manual refresh toggle, verify via CI"
echo "     (MockClock tick change → frame callback cadence adjusts)"
echo
read -rp "  Refresh rate change → cadence adjusts? (y/n/skip) " yn
case "$yn" in
    y) pass "V-16: Refresh rate change OK" ;;
    skip) echo "  ℹ️  V-16 skipped (device limitation)" ;;
    *) fail "V-16: Refresh rate change not handled" ;;
esac

# V-17: CONF race conditions
echo
echo "--- V-17: Rapid rotation → no corruption (O-04) ---"
echo "  1. Rapidly rotate tablet back and forth several times"
echo "  2. No visual corruption during transitions"
echo "  3. Final orientation shows correct content"
echo "  4. App debug page: no serial gaps or protocol errors"
echo
read -rp "  Rapid rotation → stable? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-17: Rotation race condition"; fi
pass "V-17: Race condition OK"

echo
echo "=== M5: ALL CHECKS PASSED ==="

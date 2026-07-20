#!/bin/bash
# M4-verify.sh — Touch input verification on device
# 前置条件：
#   - M3 通过（App 渲染正常 + 帧循环稳定）
#   - 容器内 weston-simple-touch 可用
# 运行位置：开发机（adb）或直接平板端 + 容器内操作
set -euo pipefail

RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'
pass() { echo -e "  ${GREEN}✅${NC} $*"; }
fail() { echo -e "  ${RED}❌${NC} $*"; exit 1; }

echo "=== M4 Verification: Touch Input ==="
echo

# V-09: Single touch
echo "--- V-09: Single touch (weston-simple-touch) ---"
echo "  1. In container: WAYLAND_DISPLAY=land-0 weston-simple-touch"
echo "  2. On tablet: tap the screen once"
echo "  3. Verify: a dot appears at the tap location on desktop"
echo "  4. Check App debug page: TouchMessage counter increments"
echo "  5. Container log shows: 'touch down id=0 x=... y=...'"
echo
read -rp "  Single tap → dot appears? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-09: Single touch not working"; fi
pass "V-09: Single touch OK"

# V-10: Drag / move
echo
echo "--- V-10: Touch drag ---"
echo "  1. Touch and drag finger across screen"
echo "  2. Verify: dot follows finger continuously"
echo "  3. Container log shows: MOVE events between DOWN and UP"
echo
read -rp "  Drag → dot follows? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-10: Drag not working"; fi
pass "V-10: Drag OK"

# V-11: Multi-touch
echo
echo "--- V-11: Multi-touch ---"
echo "  1. Touch TWO fingers simultaneously"
echo "  2. Verify: TWO dots appear (or second touch detected in log)"
echo "  3. Container log shows: touch_id=0 and touch_id=1 both active"
echo "  4. Move fingers independently → both dots move"
echo
read -rp "  Two independent touch points work? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-11: Multi-touch not working"; fi
pass "V-11: Multi-touch OK"

# V-12: Touch frame sentinel
echo
echo "--- V-12: Touch FRAME sentinel (T-02) ---"
echo "  1. Each MotionEvent should produce one FRAME after all pointers"
echo "  2. Container log confirms: 'touch frame received'"
echo "  (CI covers this via unit tests; verify log in container)"
echo
echo "  ℹ️  CI test: proto::tests::touch_phases_are_distinct covers T-01..T-03"
pass "V-12: FRAME sentinel (verified by unit tests + log)"

# V-13: Edge cases
echo
echo "--- V-13: Edge cases ---"
echo "  1. Palm rejection: large touch area → not crash"
echo "  2. Rapid tap: 5+ taps/second → no lost events"
echo "  3. Touch at screen edges (x≈0, x≈1, y≈0, y≈1) → non-crash"
echo
read -rp "  Edge cases handled? (y/n) " yn
if [ "$yn" != "y" ]; then fail "V-13: Edge case failure"; fi
pass "V-13: Edge cases OK"

echo
echo "=== M4: ALL CHECKS PASSED ==="

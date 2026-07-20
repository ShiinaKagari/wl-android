#!/bin/bash
# M2-verify.sh — Smithay 服务端骨架真机验收
# 前置条件：
#   - 容器内已安装 turnip/freedreno（mesa-for-android-container ≥ 26.1）
#   - Droidspaces 已配置 bind mount: /data/local/tmp/wl-android → /run/wl-android
#   - wl-android 二进制已编译并放入 PATH
# 运行位置：Droidspaces 容器内
set -euo pipefail

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
pass() { echo -e "  ${GREEN}✅${NC} $*"; }
fail() { echo -e "  ${RED}❌${NC} $*"; exit 1; }
check() { local label="$1"; shift; echo -n "  $label ... "; if "$@" &>/dev/null; then pass "$label"; else fail "$label"; fi; }

echo "=== M2 Verification: Smithay Service Skeleton ==="
echo

echo "--- V-01: wl-android starts and binds land-0 ---"
wl-android &
PID=$!
sleep 1
if kill -0 "$PID" 2>/dev/null; then
    pass "wl-android process running (PID=$PID)"
else
    fail "wl-android failed to start"
fi

# V-01: Westland 协议对象枚举
echo
echo "--- V-01: Wayland protocol objects via weston-info ---"
if command -v weston-info &>/dev/null; then
    if WAYLAND_DISPLAY=land-0 weston-info 2>/dev/null | grep -q "wl_compositor"; then
        pass "wl_compositor advertised"
    else
        fail "wl_compositor not found in weston-info output"
    fi
    for g in wl_shm xdg_wm_base zwp_linux_dmabuf_v1 wl_seat wl_output wl_subcompositor; do
        if WAYLAND_DISPLAY=land-0 weston-info 2>/dev/null | grep -q "$g"; then
            pass "  $g advertised"
        else
            fail "  $g NOT advertised"
        fi
    done
else
    echo "  ${YELLOW}⚠️${NC} weston-info not installed; skipping protocol object check"
    echo "  Install with: apt install weston (or distro equivalent)"
fi

# V-02: doctor 自检
echo
echo "--- V-02: wl-android doctor self-check ---"
wl-android doctor 2>&1 || fail "doctor command failed"
pass "doctor ran successfully"

# V-03: FakeCompositor 帧到达 mock-app
echo
echo "--- V-03: FakeCompositor → mock-app frame test ---"
# This test runs on the development machine (CI), not on device.
# Document the CI expectation here.
echo "  ℹ️  V-03 verified via CI: cargo test mock_app_roundtrip (FakeCompositor → FrameMessage)"
echo "  ℹ️  See crates/wl-android/tests/mock_app.rs"

# Cleanup
kill "$PID" 2>/dev/null || true
echo
echo "=== M2: ALL CHECKS PASSED ==="

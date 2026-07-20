#!/bin/bash
# M0 Container Probe — Run inside Droidspaces container
# Checks everything wl-android needs from the container environment.
set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

pass() { echo -e "  ${GREEN}✅ OK${NC} — $*"; }
fail() { echo -e "  ${RED}❌ MISSING${NC} — $*"; }
warn() { echo -e "  ${YELLOW}⚠️  WARN${NC} — $*"; }
info() { echo -e "  ℹ️  $*"; }

echo "=== M0: Container Environment Probe ==="
echo "Target: Droidspaces container, Snapdragon 8 Elite / OnePlus Pad 3"
echo "Date: $(date)"
echo

# ---- Kernel / GPU devices ----
echo "--- GPU Device Nodes ---"
if [ -e /dev/kgsl-3d0 ]; then
    pass "/dev/kgsl-3d0 exists (KGSL path)"
else
    fail "/dev/kgsl-3d0 — container cannot access GPU via KGSL"
fi

if [ -e /dev/dri/renderD128 ] || [ -e /dev/dri/card0 ]; then
    pass "/dev/dri exists (DRM path)"
else
    warn "No /dev/dri — DRM path unavailable (expected for KGSL)"
fi

echo
echo "--- DMA-BUF Heaps ---"
if [ -d /dev/dma_heap ]; then
    pass "/dev/dma_heap directory exists"
    ls /dev/dma_heap/ 2>/dev/null || warn "No heap devices inside /dev/dma_heap"
else
    fail "/dev/dma_heap — dmabuf allocation may be limited"
fi

# ---- Mesa / Vulkan ----
echo
echo "--- Vulkan (Turnip) ---"
if command -v vulkaninfo &>/dev/null; then
    pass "vulkaninfo found"
    echo
    echo "  Instance Extensions:"
    vulkaninfo 2>/dev/null | grep -i "VK_KHR_external_memory_fd" | head -1 || info "  (see full output below)"
    echo
    echo "  Device Extensions:"
    vulkaninfo 2>/dev/null | grep -i "VK_EXT_external_memory_dma_buf" | head -1 || info "  VK_EXT_external_memory_dma_buf not detected"
    vulkaninfo 2>/dev/null | grep -i "VK_KHR_external_memory_fd" | head -1 || info "  VK_KHR_external_memory_fd not detected"
    vulkaninfo 2>/dev/null | grep -i "VK_ANDROID" | head -1 || info "  no VK_ANDROID extensions"
    echo
    echo "  GPU Name:"
    vulkaninfo 2>/dev/null | grep "deviceName" | head -1 || info "  (not found)"
    echo
    echo "  API Version:"
    vulkaninfo 2>/dev/null | grep "apiVersion" | head -1 || info "  (not found)"
else
    fail "vulkaninfo not found — Mesa/turnip not installed or not in PATH"
    info "Install mesa-for-android-container >= 26.1 with a830 KGSL support"
fi

# ---- EGL / GL (KWin needs OpenGL via freedreno or zink) ----
echo
echo "--- OpenGL (freedreno / zink) ---"
if command -v eglinfo &>/dev/null; then
    pass "eglinfo found"
    echo "  EGL vendor:"
    eglinfo 2>/dev/null | grep -i "vendor" | head -3 || true
    echo "  EGL client APIs:"
    eglinfo 2>/dev/null | grep -i "client api" | head -3 || true
elif command -v es2_info &>/dev/null; then
    pass "es2_info found"
    es2_info 2>/dev/null | grep -i "version\|vendor\|renderer" | head -5 || true
else
    warn "No eglinfo/es2_info — KWin GL may not work"
fi

# ---- Wayland libs ----
echo
echo "--- Wayland ---"
if ldconfig -p 2>/dev/null | grep -q libwayland-server; then
    pass "libwayland-server found"
else
    if [ -f /usr/lib/*/libwayland-server.so ] || [ -f /usr/lib/libwayland-server.so ]; then
        pass "libwayland-server.so found on filesystem"
    else
        fail "libwayland-server not found — cannot build/run wl-android"
    fi
fi

if command -v weston-info &>/dev/null; then
    pass "weston-info found (useful for debugging)"
else
    info "weston-info not found (optional, for debugging)"
fi

# ---- Environment ----
echo
echo "--- Environment ---"
if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
    pass "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"
else
    warn "XDG_RUNTIME_DIR not set — WAYLAND_DISPLAY will use /tmp fallback"
fi

echo
echo "--- Droidspaces Bind Mount ---"
if [ -d /run/wl-android ]; then
    pass "/run/wl-android directory exists"
    ls -la /run/wl-android/ 2>/dev/null
else
    warn "/run/wl-android not found — may need Droidspaces config:"
    info "  bind mount: /data/local/tmp/wl-android → /run/wl-android"
fi

echo
echo "=== Probe Complete ==="
echo "Check the ✅/❌ marks above."
echo "Address any ❌ before proceeding to M2."
echo
echo "For M0 socket smoke test:"
echo "  1. Container: ./smoke-server  (listens on \$SMOKE_SOCKET or /tmp/m0-smoke.sock)"
echo "  2. Host (adb): adb push smoke-client /data/local/tmp/ && adb shell /data/local/tmp/smoke-client"

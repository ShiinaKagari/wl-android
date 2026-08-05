//! AHardwareBuffer slot registration for the blit path (P-12/P-13, TODO 27).
//!
//! Two acquisition paths:
//!
//! * PRIMARY — [`AhbSlot::from_swapchain_image`]: the App's Vulkan swapchain
//!   is created with `VK_SWAPCHAIN_CREATE_DEFERRED_MEMORY_ALLOCATION_BIT_KHR`
//!   (see render.rs), so each swapchain image's backing store is a
//!   `VkDeviceMemory` the App itself allocated with the AHB export handle
//!   type. `vkGetMemoryAndroidHardwareBufferANDROID` (public
//!   `VK_ANDROID_external_memory_android_hardware_buffer`, ash module
//!   `ash::android::external_memory_android_hardware_buffer`) exports that
//!   memory as an `AHardwareBuffer*`.
//! * FALLBACK — [`AhbSlot::allocate`] / [`allocate_slots`]: plain
//!   `AHardwareBuffer_allocate` for when the swapchain cannot be exported
//!   (missing `VK_EXT_swapchain_maintenance1` or the AHB extension). The App
//!   then owns the buffers anland-style; how they get to the screen is a
//!   lane-28/30 concern, not this module's.
//!
//! Registration wire flow (P-13, DESIGN.md §4.5): the caller (lane 28) sends
//! [`AhbSlot::to_tbuf_message`] on the land.sock and IMMEDIATELY afterwards
//! calls [`AhbSlot::send_registration`] on the same socket, which performs
//! `AHardwareBuffer_sendHandleToUnixSocket` — that produces exactly one
//! trailing native_handle message (wire format + SCM_RIGHTS fds) that the
//! server's `ahb_handle` module (GZ-001) parses. `AHardwareBuffer_getNativeHandle`
//! is VNDK-only and deliberately NOT used.
//!
//! Lifetime hazards (documented per adversarial review):
//! * STALE AHB: the exported AHB shadows a swapchain image. When render.rs
//!   recreates the swapchain (OUT_OF_DATE on rotation/resize), the AHB stays
//!   *valid* (refcounted) but *stale* — it is no longer presented. Lane 28
//!   MUST drop all `AhbSlot`s and re-register on every swapchain recreation
//!   (P-14: resolution change invalidates the slot pool anyway).
//! * ONE-SHOT SEND: `sendHandleToUnixSocket` consumes a native_handle
//!   snapshot; it does not establish a persistent channel. Re-registration
//!   requires a fresh `from_swapchain_image` + `send_registration` pair.
//! * RELEASE: the Vulkan spec requires the app to `AHardwareBuffer_release`
//!   the acquired reference; [`AhbHandle`]'s Drop does this.

use std::os::fd::OwnedFd;
use std::ptr::NonNull;

use ash::vk;
use wl_android_common::proto::{self, Message};

/// Loader type for `VK_ANDROID_external_memory_android_hardware_buffer`
/// (ash 0.38: vendor module is `ash::android::…`, NOT `ash::khr::…`).
pub type AhbLoader = ash::android::external_memory_android_hardware_buffer::Device;

// ============================================================================
// Pure helpers — VERBATIM copies live in .omo/start-work/ahb-harness/src/lib.rs
// (host-tested there; keep them in sync). Nothing below this line may touch
// ndk-sys or ash so the harness can compile on the host.
// ============================================================================

/// `AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM` (android/hardware_buffer.h).
/// Mirrored as a plain u32 so stride math stays host-testable.
pub(crate) const AHB_FORMAT_R8G8B8A8_UNORM: u32 = 1;
/// `AHARDWAREBUFFER_FORMAT_R8G8B8X8_UNORM`.
pub(crate) const AHB_FORMAT_R8G8B8X8_UNORM: u32 = 2;

/// Bytes per pixel for the AHB formats the blit path supports (P-12).
/// Anything else is rejected — a guessed bytes-per-pixel produces a wrong
/// byte stride and corrupts the server's dma-buf import layout.
pub(crate) fn bytes_per_pixel(format: u32) -> Option<u32> {
    match format {
        AHB_FORMAT_R8G8B8A8_UNORM | AHB_FORMAT_R8G8B8X8_UNORM => Some(4),
        _ => None,
    }
}

/// DESIGN.md §4.5: TBUF `planes[].stride` is in BYTES, converted from the
/// PIXEL stride that `AHardwareBuffer_describe` reports.
pub(crate) fn stride_bytes_from_desc(stride_pixels: u32, format: u32) -> Option<u32> {
    bytes_per_pixel(format).map(|bpp| stride_pixels * bpp)
}

/// Map an AHB describe format to the DRM fourcc the server imports (P-12:
/// v1 blit is RGBA8888-class; ABGR8888 ↔ R8G8B8A8 per DESIGN.md §4.5).
pub(crate) fn drm_format_for_ahb(format: u32) -> Option<u32> {
    match format {
        AHB_FORMAT_R8G8B8A8_UNORM => Some(proto::DRM_FORMAT_ABGR8888),
        AHB_FORMAT_R8G8B8X8_UNORM => Some(proto::DRM_FORMAT_XBGR8888),
        _ => None,
    }
}

/// Build the TBUF slot-registration message (P-13: no fds on the message
/// itself; the fd follows as a native_handle message on the same socket).
pub(crate) fn slot_message(
    slot: u32,
    width: u32,
    height: u32,
    drm_format: u32,
    stride_bytes: u32,
) -> Message {
    Message::Slot(proto::SlotBuffer::new(
        slot,
        width,
        height,
        drm_format,
        stride_bytes,
    ))
}

// ============================================================================
// NDK-bound AHB ownership
// ============================================================================

/// Owned `AHardwareBuffer*`: releases the reference on Drop (the Vulkan spec
/// mandates releasing the reference `vkGetMemoryAndroidHardwareBufferANDROID`
/// acquires; same for `AHardwareBuffer_allocate`).
///
/// `Send` is sound: `AHardwareBuffer` is reference-counted and
/// acquire/release are thread-safe; we never read or write its contents —
/// the pointer is only ever passed to NDK entry points
/// (`AHardwareBuffer_describe`, `AHardwareBuffer_sendHandleToUnixSocket`,
/// `AHardwareBuffer_release`), all documented thread-safe. `AhbSlot` lives
/// behind the session `Arc<Mutex<Inner>>` in lib.rs, so no `Sync` is needed.
struct AhbHandle(NonNull<ndk_sys::AHardwareBuffer>);

unsafe impl Send for AhbHandle {}

impl Drop for AhbHandle {
    fn drop(&mut self) {
        unsafe { ndk_sys::AHardwareBuffer_release(self.0.as_ptr()) };
    }
}

/// One blit slot: geometry/format metadata for TBUF plus the AHB whose
/// native_handle is sent to the server (P-13).
pub struct AhbSlot {
    pub slot: u32,
    pub width: u32,
    pub height: u32,
    /// DRM fourcc, derived from the describe'd AHB format (v1: ABGR8888).
    pub format: u32,
    /// BYTES — from `AHardwareBuffer_describe`'s pixel stride, never `w * 4`.
    pub stride_bytes: u32,
    /// Always `None` in the current design: the dma-buf fd lives inside the
    /// AHB and travels via `AHardwareBuffer_sendHandleToUnixSocket`; the
    /// public NDK offers no fd extraction (`AHardwareBuffer_getNativeHandle`
    /// is VNDK-only). Field retained from the stub for wire-path symmetry.
    #[allow(dead_code)]
    pub fd: Option<OwnedFd>,
    ahb: Option<AhbHandle>,
}

impl AhbSlot {
    /// FALLBACK path: allocate a standalone `AHardwareBuffer` (public NDK,
    /// API 26; minSdk is 33). Used when the swapchain cannot export AHBs
    /// (no `VK_EXT_swapchain_maintenance1` / no AHB extension on the host
    /// driver). GPU usage flags cover both the server rendering into the
    /// buffer via dma-buf import and the App sampling it for presentation.
    ///
    /// CPU_READ_OFTEN is included to force a LINEAR (uncompressed) layout on
    /// Adreno gralloc: the server's turnip must import this dma-buf, and
    /// turnip crashes (SIGSEGV) when handed a UBWC-compressed gralloc buffer
    /// (device-verified). LINEAR costs ~2x bandwidth vs UBWC but stays within
    /// the PERFORMANCE_BOUNDARIES §2 budget.
    pub fn allocate(slot: u32, width: u32, height: u32) -> Result<Self, String> {
        let usage = (ndk_sys::AHardwareBuffer_UsageFlags::AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE.0
            | ndk_sys::AHardwareBuffer_UsageFlags::AHARDWAREBUFFER_USAGE_GPU_COLOR_OUTPUT.0
            | ndk_sys::AHardwareBuffer_UsageFlags::AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN.0)
            as u64;
        let desc = ndk_sys::AHardwareBuffer_Desc {
            width,
            height,
            layers: 1,
            format: AHB_FORMAT_R8G8B8A8_UNORM,
            usage,
            stride: 0,
            rfu0: 0,
            rfu1: 0,
        };
        let mut ptr = std::ptr::null_mut();
        let rc = unsafe { ndk_sys::AHardwareBuffer_allocate(&desc, &mut ptr) };
        if rc != 0 || ptr.is_null() {
            return Err(format!(
                "ahb: AHardwareBuffer_allocate(slot={slot}, {width}x{height}) failed: rc={rc}"
            ));
        }
        Self::from_ahb(slot, width, height, ptr)
    }

    /// PRIMARY path (TODO 27): export the AHB backing a swapchain image.
    ///
    /// `memory` must be the `VkDeviceMemory` bound to swapchain image `slot`
    /// — with the deferred-allocation swapchain in render.rs that is the
    /// App-allocated, AHB-exportable memory returned by
    /// `RenderState::image_memory(slot)`. `loader` is the
    /// `VK_ANDROID_external_memory_android_hardware_buffer` device function
    /// table (it already carries the `VkDevice` handle, so no separate
    /// device parameter is accepted — passing one would only invite a
    /// device/loader mismatch).
    ///
    /// The returned AHB has a fresh reference that this slot owns and
    /// releases on Drop (Vulkan spec requirement).
    #[allow(dead_code)] // TODO 27: primary swapchain-export path, not yet wired
    pub fn from_swapchain_image(
        loader: &AhbLoader,
        slot: u32,
        memory: vk::DeviceMemory,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        if memory == vk::DeviceMemory::null() {
            return Err(format!(
                "ahb: slot {slot}: null VkDeviceMemory (swapchain not exportable? \
                 check VK_EXT_swapchain_maintenance1 + deferred allocation)"
            ));
        }
        let info = vk::MemoryGetAndroidHardwareBufferInfoANDROID::default().memory(memory);
        let ahb = unsafe { loader.get_memory_android_hardware_buffer(&info) }
            .map_err(|e| format!("ahb: vkGetMemoryAndroidHardwareBufferANDROID(slot={slot}): {e}"))?;
        let ahb = ahb as *mut ndk_sys::AHardwareBuffer;
        if ahb.is_null() {
            return Err(format!(
                "ahb: vkGetMemoryAndroidHardwareBufferANDROID(slot={slot}) returned null"
            ));
        }
        Self::from_ahb(slot, width, height, ahb)
    }

    /// Shared tail of both acquisition paths: describe the buffer and derive
    /// the wire metadata. On any failure the AHB reference is released here
    /// so error paths cannot leak it.
    fn from_ahb(
        slot: u32,
        width: u32,
        height: u32,
        ahb: *mut ndk_sys::AHardwareBuffer,
    ) -> Result<Self, String> {
        let mut desc = ndk_sys::AHardwareBuffer_Desc {
            width: 0,
            height: 0,
            layers: 0,
            format: 0,
            usage: 0,
            stride: 0,
            rfu0: 0,
            rfu1: 0,
        };
        unsafe { ndk_sys::AHardwareBuffer_describe(ahb as *const _, &mut desc) };

        let stride_bytes = stride_bytes_from_desc(desc.stride, desc.format);
        let drm_format = drm_format_for_ahb(desc.format);
        let (stride_bytes, drm_format) = match (stride_bytes, drm_format) {
            (Some(s), Some(f)) => (s, f),
            _ => {
                unsafe { ndk_sys::AHardwareBuffer_release(ahb) };
                return Err(format!(
                    "ahb: slot {slot}: unsupported AHB format {:#010x} \
                     (describe {w}x{h}, stride {} px)",
                    desc.format,
                    desc.stride,
                    w = desc.width,
                    h = desc.height,
                ));
            }
        };
        if desc.width != width || desc.height != height {
            log::warn!(
                "ahb: slot {slot}: describe {}x{} != requested {}x{} (using requested geometry)",
                desc.width, desc.height, width, height,
            );
        }
        let ahb = NonNull::new(ahb)
            .ok_or_else(|| format!("ahb: slot {slot}: null AHardwareBuffer after describe"))?;
        Ok(Self {
            slot,
            width,
            height,
            format: drm_format,
            stride_bytes,
            fd: None,
            ahb: Some(AhbHandle(ahb)),
        })
    }

    /// P-13: send this slot's AHB native_handle on `socket_fd` (the land.sock
    /// write end — the caller owns the socket; session.rs exposes the raw
    /// fd). Must be called IMMEDIATELY after the TBUF message is written, on
    /// the same socket, so the server's `ahb_handle` parser (GZ-001) sees
    /// exactly one native_handle per TBUF.
    ///
    /// One-shot: each call sends a fresh handle snapshot. Re-registration
    /// (P-14, swapchain recreation) must go through a fresh
    /// `from_swapchain_image`/`allocate` + `send_registration` pair.
    pub fn send_registration(&self, socket_fd: i32) -> Result<(), String> {
        let ahb = self.ahb.as_ref().ok_or_else(|| {
            format!("ahb: slot {} has no AHardwareBuffer to send", self.slot)
        })?;
        let rc = unsafe { ndk_sys::AHardwareBuffer_sendHandleToUnixSocket(ahb.0.as_ptr(), socket_fd) };
        if rc != 0 {
            return Err(format!(
                "ahb: AHardwareBuffer_sendHandleToUnixSocket(slot={}, fd={socket_fd}): rc={rc}",
                self.slot
            ));
        }
        Ok(())
    }

    /// TBUF protocol message for slot registration (P-13). Carries the
    /// describe'd BYTE stride; carries no fds itself.
    pub fn to_tbuf_message(&self) -> Message {
        slot_message(self.slot, self.width, self.height, self.format, self.stride_bytes)
    }

    /// Whether this slot holds a real AHB (both acquisition paths do; a
    /// slot without one can never be registered).
    #[allow(dead_code)]
    pub fn has_ahb(&self) -> bool {
        self.ahb.is_some()
    }

    /// The raw `AHardwareBuffer*` for renderer-side import (route 1: the
    /// App imports its own AHB as a VkImage and GPU-blits it into the
    /// swapchain each frame).
    pub fn raw_ahb_ptr(&self) -> Option<*mut ndk_sys::AHardwareBuffer> {
        self.ahb.as_ref().map(|h| h.0.as_ptr())
    }
}

/// FALLBACK path (see [`AhbSlot::allocate`]): build SLOT_COUNT standalone
/// AHB slots. The primary path is per-swapchain-image extraction driven by
/// lane 28 (`from_swapchain_image` + `RenderState::image_memory`); this
/// helper remains for when swapchain export is unavailable on the device.
pub fn allocate_slots(width: u32, height: u32) -> Result<Vec<AhbSlot>, String> {
    let count = proto::SLOT_COUNT as u32;
    (0..count).map(|i| AhbSlot::allocate(i, width, height)).collect()
}

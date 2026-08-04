//! ahb-harness — host-testable seams for android-app/native/src/ahb.rs (TODO 27).
//!
//! The NDK-bound parts of ahb.rs (vkGetMemoryAndroidHardwareBufferANDROID,
//! AHardwareBuffer_describe, AHardwareBuffer_sendHandleToUnixSocket) cannot be
//! host-tested. What CAN be host-tested is the pure logic:
//!   - pixel-stride → byte-stride conversion (DESIGN.md §4.5: TBUF stride is
//!     BYTES, converted from AHardwareBuffer_describe's PIXEL stride),
//!   - the TBUF message builder (P-13: TBUF carries NO fds itself; the
//!     native_handle message follows on the same socket).
//!
//! The helpers below are a VERBATIM copy of the ones in ahb.rs; if they
//! diverge, this harness is lying. Tests reference rule numbers per AGENTS.md.

// Verbatim ahb.rs helpers are consumed only by this crate's tests.
#![allow(dead_code)]

use wl_android_common::proto::{self, Message};

// ============================================================================
// Verbatim pure helpers from ahb.rs (keep in sync!)
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
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use wl_android_common::proto::{DRM_FORMAT_ABGR8888, MAGIC_TBUF, SLOT_COUNT};

    /// P-12: slot pool is triple-buffered.
    #[test]
    fn p12_slot_count_is_three() {
        assert_eq!(SLOT_COUNT, 3);
    }

    /// P-12/DESIGN §4.5: AHB describe reports stride in PIXELS; TBUF wants
    /// BYTES. R8G8B8A8/R8G8B8X8 are 4 bpp. A 1080-wide buffer whose gralloc
    /// stride is padded to 1088 px must become 4352 B on the wire.
    #[test]
    fn p12_stride_pixels_to_bytes() {
        assert_eq!(stride_bytes_from_desc(1088, 1), Some(1088 * 4)); // R8G8B8A8_UNORM
        assert_eq!(stride_bytes_from_desc(1088, 2), Some(1088 * 4)); // R8G8B8X8_UNORM
        assert_eq!(stride_bytes_from_desc(1920, 1), Some(1920 * 4));
    }

    /// P-12: formats outside the supported RGBA8888 family must be rejected,
    /// not silently mis-converted (a wrong byte-stride corrupts the server's
    /// dma-buf import layout).
    #[test]
    fn p12_unknown_format_rejected() {
        assert_eq!(stride_bytes_from_desc(1088, 0x2b), None); // R10G10B10A2: unsupported
        assert_eq!(stride_bytes_from_desc(1088, 0x21), None); // BLOB: nonsense here
        assert_eq!(stride_bytes_from_desc(1088, u32::MAX), None);
    }

    /// P-12/DESIGN §4.5: describe'd AHB formats map to the DRM fourccs the
    /// server imports (ABGR8888 ↔ R8G8B8A8_UNORM); anything else rejects.
    #[test]
    fn p12_ahb_format_to_drm_fourcc() {
        use wl_android_common::proto::{DRM_FORMAT_ABGR8888, DRM_FORMAT_XBGR8888};
        assert_eq!(drm_format_for_ahb(1), Some(DRM_FORMAT_ABGR8888));
        assert_eq!(drm_format_for_ahb(2), Some(DRM_FORMAT_XBGR8888));
        assert_eq!(drm_format_for_ahb(0x2b), None);
    }

    /// P-13: the TBUF message itself carries NO fds (fd_count == 0) — the fd
    /// follows as exactly one native_handle message sent via
    /// AHardwareBuffer_sendHandleToUnixSocket on the same socket.
    #[test]
    fn p13_tbuf_carries_no_fds() {
        let msg = slot_message(0, 1080, 2400, DRM_FORMAT_ABGR8888, 4352);
        assert_eq!(proto::fd_count(&msg), 0);
    }

    /// P-13/DESIGN §4.5: encode → decode roundtrip preserves the registered
    /// geometry and the describe'd byte stride.
    #[test]
    fn p13_tbuf_encode_roundtrip() {
        let msg = slot_message(2, 1080, 2400, DRM_FORMAT_ABGR8888, 4352);
        let bytes = proto::encode(&msg);
        assert_eq!(bytes.len(), 64); // DESIGN.md: TBUF is 64 B
        assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()), MAGIC_TBUF);

        let decoded = proto::decode(&bytes, vec![]).expect("decode TBUF");
        match decoded {
            Message::Slot(s) => {
                assert_eq!(s.slot, 2);
                assert_eq!(s.width, 1080);
                assert_eq!(s.height, 2400);
                assert_eq!(s.drm_format, DRM_FORMAT_ABGR8888);
                assert_eq!(s.num_planes, 1);
                assert_eq!(s.planes[0].offset, 0);
                assert_eq!(s.planes[0].stride, 4352);
                for p in &s.planes[1..] {
                    assert_eq!((p.offset, p.stride), (0, 0));
                }
            }
            other => panic!("expected Slot, got {other:?}"),
        }
    }
}

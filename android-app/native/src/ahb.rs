/// AHardwareBuffer management for the blit fallback path.
///
/// The App allocates AHB-backed buffers, extracts dmabuf fds,
/// and sends them to wl-android via TBUF protocol messages.
///
/// M6b: full NDK AHB API integration. For now: skeleton.

use std::os::fd::OwnedFd;

pub struct AhbSlot {
    pub slot: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub stride_bytes: u32,
    pub fd: Option<OwnedFd>,
}

impl AhbSlot {
    /// Allocate a new AHardwareBuffer and extract its dmabuf fd.
    /// Callable from Kotlin via JNI or directly from Rust via NDK.
    pub fn allocate(slot: u32, width: u32, height: u32) -> Result<Self, String> {
        // M6b: AHardwareBuffer_allocate(...) via ndk-sys or FFI
        // → AHardwareBuffer_getNativeHandle → native_handle_t.data[0] → fd
        Ok(Self {
            slot,
            width,
            height,
            format: wl_android_common::proto::DRM_FORMAT_ABGR8888,
            stride_bytes: width * 4,
            fd: None,
        })
    }

    /// Build a TBUF protocol message for slot registration.
    pub fn to_tbuf_message(&self) -> wl_android_common::proto::Message {
        wl_android_common::proto::Message::Slot(
            wl_android_common::proto::SlotBuffer::new(
                self.slot, self.width, self.height, self.format, self.stride_bytes,
            )
        )
    }
}

/// Allocate SLOT_COUNT AHB slots for blit mode.
pub fn allocate_slots(width: u32, height: u32) -> Result<Vec<AhbSlot>, String> {
    let count = wl_android_common::proto::SLOT_COUNT as u32;
    (0..count).map(|i| AhbSlot::allocate(i, width, height)).collect()
}

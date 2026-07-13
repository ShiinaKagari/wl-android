use std::sync::atomic::{AtomicU64, Ordering};

use crate::dmabuf;
use crate::socket::FrameSender;

pub struct LandBackend {
    sender: FrameSender,
    serial_counter: AtomicU64,
    _wrapped: std::ptr::NonNull<std::ffi::c_void>,
}

impl LandBackend {
    /// # SAFETY
    /// - `renderer` 必须指向有效的 `wlr_renderer`
    /// - `renderer` 在本后端生命周期内保持有效
    pub unsafe fn wrap(renderer: *mut std::ffi::c_void, sender: FrameSender) -> Result<Self, Box<dyn std::error::Error>> {
        let wrapped = std::ptr::NonNull::new(renderer)
            .ok_or("null renderer pointer")?;

        Ok(Self {
            sender,
            serial_counter: AtomicU64::new(1),
            _wrapped: wrapped,
        })
    }

    /// # SAFETY
    /// - `buffer` must point to a valid, locked `wlr_buffer`
    pub unsafe fn on_buffer_submit(&self, buffer: *mut std::ffi::c_void) -> Result<(), Box<dyn std::error::Error>> {
        // SAFETY: `buffer` is guaranteed valid by the caller (wlroots compositor)
        let meta = unsafe { dmabuf::buffer_metadata(buffer)? };
        // SAFETY: same as above; buffer is locked and supports DMA-BUF export
        let fds = unsafe { dmabuf::extract_dmabuf_fds(buffer)? };
        let serial = self.serial_counter.fetch_add(1, Ordering::AcqRel);

        self.sender.send_frame(&fds, meta.width, meta.height, meta.format, meta.stride, serial)?;

        // fds 被 consume，drop 时自动 close
        // 原始 fds 在 wlr_buffer 释放时由 wlroots 关闭
        Ok(())
    }
}

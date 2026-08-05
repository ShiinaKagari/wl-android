// Rendering via C bridge (bridge.c) — calls ANativeWindow_fromSurface through C,
// avoiding Rust JNI type mismatches on aarch64.

use std::sync::Mutex;

use log;

unsafe extern "C" {
    fn wl_get_native_window(env: *mut std::ffi::c_void, surface: jni::sys::jobject) -> *mut std::ffi::c_void;
    fn wl_lock_window(window: *mut std::ffi::c_void, buf: *mut ndk_sys::ANativeWindow_Buffer) -> std::ffi::c_int;
    fn wl_unlock_and_post(window: *mut std::ffi::c_void) -> std::ffi::c_int;
    fn wl_acquire_window(window: *mut std::ffi::c_void);
    fn wl_set_format(window: *mut std::ffi::c_void) -> std::ffi::c_int;
    fn wl_set_dimensions(window: *mut std::ffi::c_void, width: std::ffi::c_int, height: std::ffi::c_int) -> std::ffi::c_int;
}

static WINDOW: Mutex<Option<usize>> = Mutex::new(None);

/// Borrow the current ANativeWindow (as `*mut c_void`) for renderer init
/// (TODO 26). Returns None when no surface is set. Ownership stays here —
/// the caller must not release it; the window may be replaced by a later
/// `set_surface`, so use it immediately (e.g. vkCreateAndroidSurfaceKHR) and
/// don't cache it.
pub fn window_ptr() -> Option<*mut std::ffi::c_void> {
    WINDOW.lock().unwrap().filter(|w| *w != 0).map(|w| w as *mut std::ffi::c_void)
}

pub fn set_surface(env: *mut std::ffi::c_void, surface: jni::sys::jobject) {
    let mut w = WINDOW.lock().unwrap();
    if surface.is_null() {
        if let Some(old) = w.take() {
            if old != 0 {
                unsafe { ndk_sys::ANativeWindow_release(old as _); }
            }
        }
        log::info!("set_surface: null, releasing");
        return;
    }
    let window = unsafe { wl_get_native_window(env, surface) };
    if window.is_null() {
        log::error!("set_surface: wl_get_native_window returned null");
        return;
    }
    // Idempotent re-arm (SURFACE-REARM): the same window pointer being set
    // again (surfaceChanged/onResume after a restart) must NOT release +
    // re-acquire — that tears down the buffer queue on the SurfaceFlinger
    // side and the display goes black while frames still flow. Keep the
    // existing acquired window; only (re-)apply the format, which is cheap.
    if *w == Some(window as usize) {
        let fmt_result = unsafe { wl_set_format(window as _) };
        log::info!("set_surface: same window (re-arm) set_format={fmt_result}");
        return;
    }
    if let Some(old) = w.take() {
        if old != 0 {
            unsafe { ndk_sys::ANativeWindow_release(old as _); }
        }
    }
    unsafe { wl_acquire_window(window as _); }
    let fmt_result = unsafe { wl_set_format(window as _) };
    log::info!("set_surface: window={window:p} (acquired) set_format={fmt_result}");
    *w = Some(window as usize);
}

/// SCALE: set the ANativeWindow buffer geometry to the render target
/// resolution (physical × scale). The SurfaceView stays fullscreen, so
/// SurfaceFlinger stretches the smaller render buffer to fill the panel.
/// 0x0 restores fullscreen (scale 1.0). Safe to call before any lock; the
/// next ANativeWindow_lock returns the new buffer size.
pub fn set_render_size(width: u32, height: u32) {
    let w = WINDOW.lock().unwrap();
    let Some(window) = *w else { return };
    if window == 0 { return; }
    let rc = unsafe { wl_set_dimensions(window as _, width as _, height as _) };
    log::info!("set_render_size: {width}x{height} rc={rc}");
}

/// Row-wise BGRX -> BGRA copy (byte-identical: both sides are B,G,R,X memory
/// order; only the strides differ). Replaces the old per-pixel channel-swap
/// loop for `bpp == 4` windows — 8.14M pixels/frame of per-pixel work was
/// 12-80ms/frame.
///
/// Per-row clamping guarantees we never read past the end of `src`, even when
/// it is shorter than the full expected `copy_w * copy_h * 4` bytes (e.g. an
/// fstat-truncated SHM frame, TODO 7), and never write past `dst`.
///
/// Returns `true` if a truncated source or destination was detected and one or
/// more rows were clamped or skipped (caller logs a warning).
fn copy_row_bgra(
    dst: &mut [u8],
    dst_stride_bytes: usize,
    src: &[u8],
    src_stride_bytes: usize,
    copy_w: usize,
    copy_h: usize,
) -> bool {
    let row_bytes = copy_w * 4;
    // PERF: when both strides match (the common case: window stride ==
    // SHM stride == width*4), the whole frame is one contiguous block —
    // a single memcpy beats per-row copies (less loop overhead, better
    // vectorization). Fall back to row-wise when they differ.
    if src_stride_bytes == dst_stride_bytes {
        let total = row_bytes.saturating_mul(copy_h);
        let n = total.min(src.len()).min(dst.len());
        // SAFETY: both slices are valid; n is clamped to both lengths.
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr(), n);
        }
        return n < total;
    }
    let mut truncated = false;
    for y in 0..copy_h {
        let src_row = y * src_stride_bytes;
        if src_row >= src.len() {
            truncated = true;
            continue;
        }
        let n = row_bytes.min(src.len() - src_row);
        if n < row_bytes {
            truncated = true;
        }
        if n == 0 {
            continue;
        }
        let dst_row = y * dst_stride_bytes;
        if dst_row >= dst.len() {
            truncated = true;
            continue;
        }
        let n = n.min(dst.len() - dst_row);
        unsafe {
            // `src` (SHM frame) and `dst` (window buffer) are distinct
            // allocations, so the regions never overlap.
            std::ptr::copy_nonoverlapping(
                src.as_ptr().add(src_row),
                dst.as_mut_ptr().add(dst_row),
                n,
            );
        }
    }
    truncated
}

/// CPU fallback render path (legacy SHM frames / swapchain-unavailable
/// devices): BGRX→BGRA row copy into the ANativeWindow via
/// `ANativeWindow_lock`. Deprecated in favor of the Vulkan swapchain present
/// path (P3, TODO 30): when the swapchain is up, fence frames bypass this
/// entirely, and this function only serves pre-blit SHM servers or a
/// failed/absent swapchain init. Kept working — it is the non-swapchain
/// fallback.
#[deprecated(note = "CPU fallback path — swapchain present is primary (P3)")]
pub fn render_frame(serial: u64, width: u32, height: u32, pixel_data: &[u8]) -> Result<(), String> {
    let guard = WINDOW.lock().unwrap();
    let window = match *guard {
        Some(w) if w != 0 => w,
        _ => { log::warn!("render_frame: no window"); return Err("no window".into()); }
    };

    let mut buf: ndk_sys::ANativeWindow_Buffer = unsafe { std::mem::zeroed() };
    let lock_result = unsafe { wl_lock_window(window as _, &mut buf) };
    if lock_result != 0 {
        log::error!("render_frame: ANativeWindow_lock failed with {}", lock_result);
        return Err(format!("lock failed: {}", lock_result));
    }
    if buf.width == 0 || buf.height == 0 {
        unsafe { wl_unlock_and_post(window as _); }
        log::error!("render_frame: bad dimensions {}x{}", buf.width, buf.height);
        return Err("zero dimensions".into());
    }

    log::info!("render: buf {}x{} stride={} fmt={:#x} bits={:p}", buf.width, buf.height, buf.stride, buf.format, buf.bits);

    let bpp = match buf.format {
        1 => 4, // WINDOW_FORMAT_RGBA_8888
        2 => 4, // WINDOW_FORMAT_RGBX_8888
        4 => 2, // WINDOW_FORMAT_RGB_565
        _ => 4,
    };

    if pixel_data.is_empty() {
        let stride = buf.stride as usize;
        let bits = buf.bits as *mut u8;
        let max_h = buf.height.min(50) as usize;
        let max_w = buf.width.min(50) as usize;
        for y in 0..max_h {
            for x in 0..max_w {
                let off = y * stride * bpp + x * bpp;
                unsafe {
                    if bpp == 4 {
                        *bits.add(off) = 0;
                        *bits.add(off + 1) = 0;
                        *bits.add(off + 2) = 0;
                        *bits.add(off + 3) = 0xFF;
                    } else {
                        let rgb565: u16 = 0xF800;
                        *bits.add(off) = (rgb565 & 0xFF) as u8;
                        *bits.add(off + 1) = ((rgb565 >> 8) & 0xFF) as u8;
                    }
                }
            }
        }
    } else {
        let dst_stride = buf.stride as usize;
        let src_stride = (width as usize) * 4;
        let dst_bits = buf.bits as *mut u8;
        let copy_w = (buf.width as usize).min(width as usize);
        let copy_h = (buf.height as usize).min(height as usize);
        if bpp == 4 {
            // Fast path: the window is now WINDOW_FORMAT_BGRA_8888 (TODO 9),
            // whose buffer is B,G,R,X memory order — byte-identical to the
            // KWin SHM BGRX frames. Only the strides differ, so a per-row
            // memcpy replaces the old per-pixel channel-swap loop.
            let dst_len = dst_stride * copy_h * bpp;
            let dst_slice = unsafe { std::slice::from_raw_parts_mut(dst_bits, dst_len) };
            if copy_row_bgra(dst_slice, dst_stride * bpp, pixel_data, src_stride, copy_w, copy_h) {
                log::warn!(
                    "render_frame: SHM frame truncated ({}B < {}B expected); rows clamped",
                    pixel_data.len(),
                    copy_w * copy_h * 4
                );
            }
        } else {
            // RGB_565 fallback (old window format): keep per-pixel conversion.
            for y in 0..copy_h {
                for x in 0..copy_w {
                    let src_off = y * src_stride + x * 4;
                    let dst_off = y * dst_stride * bpp + x * bpp;
                    let b = pixel_data[src_off];
                    let g = pixel_data[src_off + 1];
                    let r = pixel_data[src_off + 2];
                    unsafe {
                        let r5 = ((r as u16) >> 3) & 0x1F;
                        let g6 = ((g as u16) >> 2) & 0x3F;
                        let b5 = ((b as u16) >> 3) & 0x1F;
                        let rgb565: u16 = (r5 << 11) | (g6 << 5) | b5;
                        *dst_bits.add(dst_off) = (rgb565 & 0xFF) as u8;
                        *dst_bits.add(dst_off + 1) = ((rgb565 >> 8) & 0xFF) as u8;
                    }
                }
            }
        }
    }

    unsafe { wl_unlock_and_post(window as _); }
    log::debug!("render_frame: serial={serial} {}x{}", width, height);
    Ok(())
}

/// RENDER-DECOUPLE: render a frame from its pixel fd (the recv thread enqueued
/// the fd; this runs on the dedicated render thread). fstat-guarded mmap of
/// the fd, then the same BGRX→BGRA row copy as `render_frame`, then munmap.
pub fn render_frame_fd(
    serial: u64,
    width: u32,
    height: u32,
    pixel_fd: &std::os::fd::OwnedFd,
) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let size = width as usize * height as usize * 4;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(pixel_fd.as_raw_fd(), &mut st) } != 0 {
        log::error!("render_frame_fd: fstat failed; dropping frame");
        return Err("fstat failed".into());
    }
    let map_len = (st.st_size as usize).min(size);
    if map_len == 0 {
        log::warn!("render_frame_fd: empty fd; dropping frame");
        return Err("empty fd".into());
    }
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            map_len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            pixel_fd.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        log::error!("render_frame_fd: mmap failed; dropping frame");
        return Err("mmap failed".into());
    }
    // SAFETY: ptr is a live readable mapping of map_len bytes (fstat-guarded).
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, map_len) };
    #[allow(deprecated)]
    let result = render_frame(serial, width, height, slice);
    unsafe { libc::munmap(ptr, map_len); }
    result
}

/// CONN-STATE: blank the ANativeWindow to pure black. Called by the render
/// thread when the session is Disconnected (server died) so the display does
/// not keep showing a stale frozen frame. No-op when no window is set.
pub fn blank_screen() {
    let guard = WINDOW.lock().unwrap();
    let window = match *guard {
        Some(w) if w != 0 => w,
        _ => return,
    };
    let mut buf: ndk_sys::ANativeWindow_Buffer = unsafe { std::mem::zeroed() };
    if unsafe { wl_lock_window(window as _, &mut buf) } != 0 {
        return;
    }
    if buf.width == 0 || buf.height == 0 {
        unsafe { wl_unlock_and_post(window as _); }
        return;
    }
    let bpp = match buf.format {
        4 => 2,
        _ => 4,
    };
    let stride = buf.stride as usize * bpp;
    let bits = buf.bits as *mut u8;
    let total = stride * buf.height as usize;
    // SAFETY: bits is the locked window buffer of `total` bytes.
    unsafe { std::ptr::write_bytes(bits, 0, total); }
    unsafe { wl_unlock_and_post(window as _); }
    log::info!("blank_screen: display blanked (disconnected)");
}

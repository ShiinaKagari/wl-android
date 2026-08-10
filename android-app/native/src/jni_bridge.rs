// Rendering via C bridge (bridge.c) — calls ANativeWindow_fromSurface through C,
// avoiding Rust JNI type mismatches on aarch64.

use std::sync::Mutex;

unsafe extern "C" {
    fn wl_get_native_window(env: *mut std::ffi::c_void, surface: jni::sys::jobject) -> *mut std::ffi::c_void;
    fn wl_lock_window(window: *mut std::ffi::c_void, buf: *mut ndk_sys::ANativeWindow_Buffer) -> std::ffi::c_int;
    fn wl_unlock_and_post(window: *mut std::ffi::c_void) -> std::ffi::c_int;
    fn wl_acquire_window(window: *mut std::ffi::c_void);
    fn wl_set_format(window: *mut std::ffi::c_void) -> std::ffi::c_int;
    fn wl_set_dimensions(window: *mut std::ffi::c_void, width: std::ffi::c_int, height: std::ffi::c_int) -> std::ffi::c_int;
    /// AHB-PROBE: try to import an external dmabuf fd into an AHardwareBuffer
    /// via gralloc. Returns 0 (API absent), 1 (import OK), or -errno (failed).
    /// On success, `pixel_probe` (16 bytes) receives the first pixels read
    /// back through AHardwareBuffer_lock, proving the buffer is GPU-reachable.
    fn wl_probe_ahardwarebuffer_import(
        dmabuf_fd: std::ffi::c_int,
        width: std::ffi::c_int,
        height: std::ffi::c_int,
        stride: std::ffi::c_int,
        pixel_probe: *mut u8,
    ) -> std::ffi::c_int;
}

/// AHB-PROBE (GLOBAL): exposes the probe to lib.rs. Runs on the recv thread
/// for the first few dmabuf frames only.
pub fn probe_ahardwarebuffer_import(
    fd: &std::os::fd::OwnedFd,
    width: u32,
    height: u32,
    stride: u32,
) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let mut pixel_probe = [0u8; 16];
    let rc = unsafe {
        wl_probe_ahardwarebuffer_import(
            fd.as_raw_fd(),
            width as _,
            height as _,
            stride as _,
            pixel_probe.as_mut_ptr(),
        )
    };
    match rc {
        0 => log::info!("AHB-PROBE: createFromHandle API not present (0)"),
        1 => log::info!("AHB-PROBE: import OK — first pixels {:02x?}", &pixel_probe),
        err => log::info!("AHB-PROBE: import failed rc={err} (createFromHandle={rc})"),
    }
    Ok(())
}

static WINDOW: Mutex<Option<usize>> = Mutex::new(None);

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
/// fstat-truncated SHM frame), and never write past `dst`.
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

/// Render a frame's pixels into the ANativeWindow via `ANativeWindow_lock`.
/// `pixel_data` is a full frame (dmabuf or SHM pixel fd, mmap'd by the
/// caller); B,G,R,X memory order (window format is fixed to BGRA_8888 by
/// wl_set_format, byte-identical to the KWin BGRX frames — only strides
/// differ, handled by copy_row_bgra).
pub fn render_frame(width: u32, height: u32, pixel_data: &[u8]) -> Result<(), String> {
    let guard = WINDOW.lock().unwrap();
    let window = match *guard {
        Some(w) if w != 0 => w,
        _ => { log::warn!("render_frame: no window"); return Err("no window".into()); }
    };

    let mut buf: ndk_sys::ANativeWindow_Buffer = unsafe { std::mem::zeroed() };
    let t_lock = std::time::Instant::now();
    let lock_result = unsafe { wl_lock_window(window as _, &mut buf) };
    let lock_ms = t_lock.elapsed().as_millis();
    if lock_result != 0 {
        log::error!("render_frame: ANativeWindow_lock failed with {}", lock_result);
        return Err(format!("lock failed: {}", lock_result));
    }
    if lock_ms > 30 {
        // DIAG-RENDER: lock blocking >30ms is abnormal (SurfaceFlinger
        // backpressure) and would directly explain a slow render thread.
        log::warn!("render_frame: ANativeWindow_lock took {lock_ms}ms");
    }
    if buf.width == 0 || buf.height == 0 {
        unsafe { wl_unlock_and_post(window as _); }
        log::error!("render_frame: bad dimensions {}x{}", buf.width, buf.height);
        return Err("zero dimensions".into());
    }

    log::debug!("render: buf {}x{} stride={} fmt={:#x} bits={:p}", buf.width, buf.height, buf.stride, buf.format, buf.bits);

    // Window format is pinned to BGRA_8888 by wl_set_format (bridge.c), so
    // the buffer is always 4 bytes/pixel (B,G,R,X order). RGB_565 fallback
    // was removed — the format contract never changes.
    const BPP: usize = 4;
    let dst_stride = buf.stride as usize;
    let src_stride = (width as usize) * 4;
    let dst_bits = buf.bits as *mut u8;
    let copy_w = (buf.width as usize).min(width as usize);
    let copy_h = (buf.height as usize).min(height as usize);
    // Fast path: byte-identical B,G,R,X memory order; only strides differ,
    // so a row-wise memcpy replaces the old per-pixel channel-swap loop.
    let dst_len = dst_stride * copy_h * BPP;
    let t_copy = std::time::Instant::now();
    let dst_slice = unsafe { std::slice::from_raw_parts_mut(dst_bits, dst_len) };
    if copy_row_bgra(dst_slice, dst_stride * BPP, pixel_data, src_stride, copy_w, copy_h) {
        log::warn!(
            "render_frame: frame truncated ({}B < {}B expected); rows clamped",
            pixel_data.len(),
            copy_w * copy_h * 4
        );
    }
    let copy_ms = t_copy.elapsed().as_millis();

    unsafe { wl_unlock_and_post(window as _); }
    // DIAG-RENDER: per-frame cost split — lock (SurfaceFlinger) vs copy
    // (memcpy). Logged every 60 frames (rate-limited).
    {
        use std::sync::atomic::{AtomicU32, Ordering};
        static FRAMES: AtomicU32 = AtomicU32::new(0);
        let n = FRAMES.fetch_add(1, Ordering::Relaxed);
        if n % 60 == 0 {
            log::info!("render_frame: lock={lock_ms}ms copy={copy_ms}ms total={}x{}", width, height);
        }
    }
    log::debug!("render_frame: {}x{}", width, height);
    Ok(())
}

/// RENDER-DECOUPLE: per-buffer mmap cache. KWin's buffer pool is small
/// (2-3 dmabufs) and rotates, so caching the mapping per BUFFER avoids
/// mmap+munmap of the whole frame (32MB page-table churn ≈ 20ms) on every
/// frame.
///
/// The key is the dmabuf's (st_dev, st_ino) identity, NOT the fd number:
/// the server dups a fresh fd per frame, and the kernel reuses closed fd
/// numbers — a raw fd-keyed cache would hit a stale mapping of a DIFFERENT
/// buffer and freeze the picture (only a buffer-pool rotation that happens
/// to use a fresh fd number would refresh it). Same-inode mappings are
/// MAP_SHARED, so their contents follow the buffer's updates.
///
/// Owned by the render thread only — no locking needed. The mappings live
/// for the process lifetime (bounded by the buffer pool size).
pub struct FdMmapCache {
    maps: std::collections::HashMap<(u64, u64), (usize, *mut u8)>,
}

// SAFETY: only used from the render thread; pointers are into private
// mmaps that stay valid for the cache's lifetime.
unsafe impl Send for FdMmapCache {}

impl FdMmapCache {
    pub fn new() -> Self {
        Self { maps: std::collections::HashMap::new() }
    }

    /// Map `fd` if its backing buffer is not cached; returns (ptr, len).
    fn map(&mut self, fd: i32, len: usize) -> Option<(*const u8, usize)> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            log::error!("FdMmapCache: fstat failed for fd {fd}");
            return None;
        }
        let key = (st.st_dev, st.st_ino);
        if let Some(&(cached_len, ptr)) = self.maps.get(&key) {
            return Some((ptr as *const u8, cached_len));
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            log::error!("FdMmapCache: mmap failed for fd {fd}");
            return None;
        }
        self.maps.insert(key, (len, ptr as *mut u8));
        Some((ptr as *const u8, len))
    }
}

/// SNAPSHOT-POOL: copy the frame's pixels out of the shared mapping into
/// `out` IMMEDIATELY (recv thread, before KWin can rewrite the buffer under
/// a delayed release). The private snapshot then feeds the display copy at
/// leisure. Returns () on success.
pub fn snapshot_frame_into(
    width: u32,
    height: u32,
    pixel_fd: &std::os::fd::OwnedFd,
    cache: &mut FdMmapCache,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let size = width as usize * height as usize * 4;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(pixel_fd.as_raw_fd(), &mut st) } != 0 {
        log::error!("snapshot_frame_into: fstat failed; dropping frame");
        return Err("fstat failed".into());
    }
    let map_len = (st.st_size as usize).min(size);
    if map_len == 0 {
        log::warn!("snapshot_frame_into: empty fd; dropping frame");
        return Err("empty fd".into());
    }
    let (ptr, _) = cache.map(pixel_fd.as_raw_fd(), map_len).ok_or_else(|| "mmap failed".to_string())?;
    // SAFETY: ptr is a live readable mapping of map_len bytes (fstat-guarded,
    // cached by fd). Copy is immediate — the shared mapping is released as
    // soon as this returns (RELEASE), so KWin can rewrite freely after.
    let slice = unsafe { std::slice::from_raw_parts(ptr, map_len) };
    out.resize(map_len, 0u8);
    out[..map_len].copy_from_slice(slice);
    Ok(())
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
    // Window format is pinned to BGRA_8888 (4 bytes/pixel) by wl_set_format.
    let stride = buf.stride as usize * 4;
    let bits = buf.bits as *mut u8;
    let total = stride * buf.height as usize;
    // SAFETY: bits is the locked window buffer of `total` bytes.
    unsafe { std::ptr::write_bytes(bits, 0, total); }
    unsafe { wl_unlock_and_post(window as _); }
    log::info!("blank_screen: display blanked (disconnected)");
}

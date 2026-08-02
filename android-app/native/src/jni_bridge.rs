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
}

static WINDOW: Mutex<Option<usize>> = Mutex::new(None);

pub fn set_surface(env: *mut std::ffi::c_void, surface: jni::sys::jobject) {
    let mut w = WINDOW.lock().unwrap();
    if let Some(old) = w.take() {
        if old != 0 {
            unsafe { ndk_sys::ANativeWindow_release(old as _); }
            unsafe { ndk_sys::ANativeWindow_release(old as _); }
        }
    }
    if surface.is_null() {
        log::info!("set_surface: null, releasing");
        return;
    }
    let window = unsafe { wl_get_native_window(env, surface) };
    if window.is_null() {
        log::error!("set_surface: wl_get_native_window returned null");
        return;
    }
    unsafe { wl_acquire_window(window as _); }
    let fmt_result = unsafe { wl_set_format(window as _) };
    log::info!("set_surface: window={window:p} (acquired) set_format={fmt_result}");
    *w = Some(window as usize);
}

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
        for y in 0..copy_h {
            for x in 0..copy_w {
                let src_off = y * src_stride + x * 4;
                let dst_off = y * dst_stride * bpp + x * bpp;
                let b = pixel_data[src_off];     // BGRX: byte0=B
                let g = pixel_data[src_off + 1]; // byte1=G
                let r = pixel_data[src_off + 2]; // byte2=R
                unsafe {
                    if bpp == 4 {
                        *dst_bits.add(dst_off) = r;       // RGBA: byte0=R
                        *dst_bits.add(dst_off + 1) = g;   // byte1=G
                        *dst_bits.add(dst_off + 2) = b;   // byte2=B
                        *dst_bits.add(dst_off + 3) = 0xFF; // byte3=A
                    } else {
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

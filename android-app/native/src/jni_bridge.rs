// Rendering via C bridge (bridge.c) — calls ANativeWindow_fromSurface through C,
// avoiding Rust JNI type mismatches on aarch64.

use std::sync::Mutex;

use log;

unsafe extern "C" {
    fn wl_get_native_window(env: *mut std::ffi::c_void, surface: jni::sys::jobject) -> *mut std::ffi::c_void;
    fn wl_lock_window(window: *mut std::ffi::c_void, buf: *mut ndk_sys::ANativeWindow_Buffer);
    fn wl_unlock_and_post(window: *mut std::ffi::c_void);
}

static WINDOW: Mutex<Option<usize>> = Mutex::new(None);

pub fn set_surface(env: *mut std::ffi::c_void, surface: jni::sys::jobject) {
    let mut w = WINDOW.lock().unwrap();
    if let Some(old) = w.take() {
        if old != 0 { unsafe { ndk_sys::ANativeWindow_release(old as _); } }
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
    log::info!("set_surface: window={window:p}");
    *w = Some(window as usize);
}

pub fn render_frame(serial: u64) -> Result<(), String> {
    let guard = WINDOW.lock().unwrap();
    let window = match *guard {
        Some(w) if w != 0 => w,
        _ => { log::warn!("render_frame: no window"); return Err("no window".into()); }
    };
    drop(guard);

    let mut buf: ndk_sys::ANativeWindow_Buffer = unsafe { std::mem::zeroed() };
    unsafe { wl_lock_window(window as _, &mut buf); }
    if buf.width == 0 || buf.height == 0 {
        log::error!("render_frame: bad dimensions {}x{}", buf.width, buf.height);
        return Err("zero dimensions".into());
    }

    let c = match serial % 4 {
        0 => 0xFF_0000FF, 1 => 0xFF_00FF00, 2 => 0xFF_FF0000, _ => 0xFF_00FFFF,
    };
    // Only fill a 200x200 rect to keep render fast (<1ms)
    let stride = buf.stride as usize;
    let bits = buf.bits as *mut u32;
    let max_h = buf.height.min(200) as usize;
    let max_w = buf.width.min(200) as usize;
    for y in 0..max_h {
        for x in 0..max_w {
            unsafe { *bits.add(y * stride + x) = c; }
        }
    }

    unsafe { wl_unlock_and_post(window as _); }
    log::debug!("render_frame: serial={serial} {}x{}", buf.width, buf.height);
    Ok(())
}

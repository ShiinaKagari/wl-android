// Rendering via C bridge — calls ANativeWindow_fromSurface safely through C,
// avoiding Rust JNI type mismatches on aarch64.

use std::sync::Mutex;

// C functions from bridge.c
unsafe extern "C" {
    fn wl_get_native_window(env: *mut std::ffi::c_void, surface: jni::sys::jobject) -> *mut std::ffi::c_void;
    fn wl_lock_window(window: *mut std::ffi::c_void, buf: *mut ndk_sys::ANativeWindow_Buffer);
    fn wl_unlock_and_post(window: *mut std::ffi::c_void);
}

static WINDOW: Mutex<Option<usize>> = Mutex::new(None);

/// Called from JNI thread (nativeSetSurface) to create/store the ANativeWindow.
pub fn set_surface(env: *mut std::ffi::c_void, surface: jni::sys::jobject) {
    let mut w = WINDOW.lock().unwrap();
    if let Some(old) = w.take() {
        if old != 0 {
            unsafe { ndk_sys::ANativeWindow_release(old as _); }
        }
    }
    if !surface.is_null() {
        let window = unsafe { wl_get_native_window(env, surface) };
        if !window.is_null() {
            *w = Some(window as usize);
        }
    }
}

/// Called from session thread on frame arrival.
#[cfg(target_os = "android")]
pub fn render_frame(serial: u64) -> Result<(), String> {
    let guard = WINDOW.lock().unwrap();
    let window = guard.ok_or("no window")?;
    if window == 0 { return Err("no window".into()); }

    let mut buf: ndk_sys::ANativeWindow_Buffer = unsafe { std::mem::zeroed() };
    unsafe { wl_lock_window(window as _, &mut buf); }

    let c = match serial % 4 {
        0 => 0xFF_0000FF, 1 => 0xFF_00FF00, 2 => 0xFF_FF0000, _ => 0xFF_00FFFF,
    };
    let pixels = buf.width as usize * buf.height as usize;
    let bits = buf.bits as *mut u32;
    for i in 0..pixels {
        unsafe { *bits.add(i) = c; }
    }

    unsafe { wl_unlock_and_post(window as _); }
    Ok(())
}

#[cfg(not(target_os = "android"))]
pub fn render_frame(_serial: u64) -> Result<(), String> { Ok(()) }

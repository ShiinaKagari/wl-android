// Surface rendering helpers.
// nativeSetSurface (JNI thread) calls ANativeWindow_fromSurface and stores the pointer.
// render_frame (session thread) uses the stored ANativeWindow* directly — no JNI needed.

use std::sync::Mutex;
use std::ptr::null_mut;

type ANativeWindowPtr = *mut std::ffi::c_void;
static WINDOW: Mutex<Option<ANativeWindowPtr>> = Mutex::new(None);

/// Called from JNI thread when surface is created/destroyed.
pub fn set_surface(env: *mut std::ffi::c_void, surface: jni::sys::jobject) {
    let mut w = WINDOW.lock().unwrap();
    // Release old window
    if let Some(old) = w.take() {
        unsafe { ndk_sys::ANativeWindow_release(old as _); }
    }
    if !surface.is_null() {
        let env_ptr: *mut std::ffi::c_void = env;
        let window = unsafe {
            ndk_sys::ANativeWindow_fromSurface(&env_ptr as *const _ as *mut _, surface)
        };
        if !window.is_null() {
            *w = Some(window as _);
        }
    }
}

/// Called from session thread on frame arrival. Uses stored ANativeWindow* directly.
#[cfg(target_os = "android")]
pub fn render_frame(serial: u64) -> Result<(), String> {
    let guard = WINDOW.lock().unwrap();
    let window = guard.ok_or("no window")?;

    let mut buf: ndk_sys::ANativeWindow_Buffer = unsafe { std::mem::zeroed() };
    if unsafe { ndk_sys::ANativeWindow_lock(window as _, &mut buf, null_mut()) } != 0 {
        return Err("lock failed".into());
    }

    let c = match serial % 4 {
        0 => 0xFF_0000FF, // blue
        1 => 0xFF_00FF00, // green
        2 => 0xFF_FF0000, // red
        _ => 0xFF_00FFFF, // cyan
    };
    let pixels = buf.width as usize * buf.height as usize;
    let bits = buf.bits as *mut u32;
    for i in 0..pixels {
        unsafe { *bits.add(i) = c; }
    }

    unsafe { ndk_sys::ANativeWindow_unlockAndPost(window as _); }
    Ok(())
}

#[cfg(not(target_os = "android"))]
pub fn render_frame(_serial: u64) -> Result<(), String> { Ok(()) }

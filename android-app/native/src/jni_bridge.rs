// Surface rendering helpers using raw ANativeWindow.
// These functions are called from the session thread after nativeSetSurface
// has stored the env + surface in static variables.

use std::sync::Mutex;
use std::ptr::null_mut;

static SURFACE: Mutex<Option<usize>> = Mutex::new(None);
static JNI_ENV: Mutex<Option<usize>> = Mutex::new(None);

pub fn set_surface(env: *mut std::ffi::c_void, surface: jni::sys::jobject) {
    *SURFACE.lock().unwrap() = if surface.is_null() { None } else { Some(surface as usize) };
    *JNI_ENV.lock().unwrap() = Some(env as usize);
}

#[cfg(target_os = "android")]
pub fn render_frame(serial: u64) -> Result<(), String> {
    let surface = SURFACE.lock().unwrap().map(|s| s as jni::sys::jobject);
    let env = JNI_ENV.lock().unwrap().map(|e| e as *mut std::ffi::c_void);
    let (surface, env) = match (surface, env) {
        (Some(s), Some(e)) => (s, e),
        _ => return Err("no surface".into()),
    };

    let env_ptr: *mut std::ffi::c_void = env;
    let window = unsafe {
        ndk_sys::ANativeWindow_fromSurface(&env_ptr as *const _ as *mut _, surface)
    };
    if window.is_null() { return Err("no window".into()); }

    let mut buf: ndk_sys::ANativeWindow_Buffer = unsafe { std::mem::zeroed() };
    if unsafe { ndk_sys::ANativeWindow_lock(window, &mut buf, null_mut()) } != 0 {
        unsafe { ndk_sys::ANativeWindow_release(window); }
        return Err("lock failed".into());
    }

    // Alternating color per serial
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

    unsafe { ndk_sys::ANativeWindow_unlockAndPost(window); }
    unsafe { ndk_sys::ANativeWindow_release(window); }
    Ok(())
}

#[cfg(not(target_os = "android"))]
pub fn render_frame(_serial: u64) -> Result<(), String> { Ok(()) }

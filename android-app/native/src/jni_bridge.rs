// Surface rendering helpers.
// Stores the ANativeWindow pointer from nativeSetSurface (JNI thread).
// render_frame uses the stored pointer directly from the session thread.
//
// For the JNI env: we take &env (the JNI env pointer on the stack)
// and cast to *mut JNIEnv which is what ANativeWindow_fromSurface expects.

use std::sync::Mutex;
use std::ptr::null_mut;

static WINDOW: Mutex<Option<usize>> = Mutex::new(None);

pub fn set_surface(env: jni::sys::JNIEnv, surface: jni::sys::jobject) {
    let mut w = WINDOW.lock().unwrap();
    if let Some(old) = w.take() {
        if old != 0 {
            unsafe { ndk_sys::ANativeWindow_release(old as *mut ndk_sys::ANativeWindow); }
        }
    }
    if !surface.is_null() {
        // ANativeWindow_fromSurface(env: *mut JNIEnv, surface: jobject)
        // env on our stack → take its address → cast to *mut JNIEnv
        let env_addr: *mut jni::sys::JNIEnv = &env as *const _ as *mut _;
        let window = unsafe {
            ndk_sys::ANativeWindow_fromSurface(env_addr, surface)
        };
        if !window.is_null() {
            *w = Some(window as usize);
        }
    }
}

#[cfg(target_os = "android")]
pub fn render_frame(serial: u64) -> Result<(), String> {
    let guard = WINDOW.lock().unwrap();
    let window = guard.ok_or("no window")?;
    if window == 0 { return Err("no window".into()); }

    let mut buf: ndk_sys::ANativeWindow_Buffer = unsafe { std::mem::zeroed() };
    if unsafe { ndk_sys::ANativeWindow_lock(window as _, &mut buf, null_mut()) } != 0 {
        return Err("lock failed".into());
    }

    let c = match serial % 4 {
        0 => 0xFF_0000FF, 1 => 0xFF_00FF00, 2 => 0xFF_FF0000, _ => 0xFF_00FFFF,
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

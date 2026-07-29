use jni::sys::{jobject, JNIEnv as RawJNIEnv};

/// Fill ANativeWindow from raw JNI pointers.
#[cfg(target_os = "android")]
pub fn fill_surface_blue(env: *mut RawJNIEnv, surface: jobject) -> Result<(), String> {
    let window = unsafe {
        ndk_sys::ANativeWindow_fromSurface(env as *mut std::ffi::c_void, surface)
    };
    if window.is_null() {
        return Err("ANativeWindow_fromSurface returned null".into());
    }

    let mut buf: ndk_sys::ANativeWindow_Buffer = unsafe { std::mem::zeroed() };
    let rc = unsafe { ndk_sys::ANativeWindow_lock(window, &mut buf, std::ptr::null_mut()) };
    if rc != 0 {
        unsafe { ndk_sys::ANativeWindow_release(window); }
        return Err(format!("ANativeWindow_lock failed: {rc}"));
    }

    let color: u32 = 0xFF_0000FF; // B8G8R8A8 opaque blue
    let pixel_count = buf.width as usize * buf.height as usize;
    let bits = buf.bits as *mut u32;
    for i in 0..pixel_count {
        unsafe { *bits.add(i) = color; }
    }

    unsafe { ndk_sys::ANativeWindow_unlockAndPost(window); }
    unsafe { ndk_sys::ANativeWindow_release(window); }
    Ok(())
}

use jni::objects::JObject;
use jni::JNIEnv;

/// Get ANativeWindow from a Surface Java object using NDK.
#[cfg(target_os = "android")]
pub fn fill_surface_blue(_env: &mut JNIEnv, surface: &JObject) -> Result<(), String> {
    use std::ptr::NonNull;
    
    // Get ANativeWindow from Surface
    let window = unsafe {
        ndk_sys::ANativeWindow_fromSurface(
            _env.get_native_interface() as *mut _,
            surface.as_raw()
        )
    };
    if window.is_null() {
        return Err("ANativeWindow_fromSurface returned null".into());
    }

    // Lock and fill with blue
    let mut out_buf: ndk_sys::ANativeWindow_Buffer = unsafe { std::mem::zeroed() };
    let mut rect = ndk_sys::ARect { left: 0, top: 0, right: 0, bottom: 0 };
    let rc = unsafe { ndk_sys::ANativeWindow_lock(window, &mut out_buf, &mut rect) };
    if rc != 0 {
        unsafe { ndk_sys::ANativeWindow_release(window); }
        return Err(format!("ANativeWindow_lock failed: {rc}"));
    }

    // Fill with solid blue (B8G8R8A8: 0xFF0000FF = opaque red in RGBA byte order)
    let color: u32 = 0xFF_0000FF; // B=FF, G=00, R=00, A=FF → pure red
    let pixels = out_buf.width as usize * out_buf.height as usize;
    let ptr = out_buf.bits as *mut u32;
    for i in 0..pixels {
        unsafe { *ptr.add(i) = color; }
    }

    unsafe { ndk_sys::ANativeWindow_unlockAndPost(window); }
    unsafe { ndk_sys::ANativeWindow_release(window); }
    Ok(())
}

#[cfg(not(target_os = "android"))]
pub fn fill_surface_blue(_env: &mut JNIEnv, _surface: &JObject) -> Result<(), String> {
    Ok(())
}

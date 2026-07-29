use jni::JNIEnv;

#[cfg(target_os = "android")]
use jni::objects::JObject;

/// Fill the ANativeWindow backing `surface` with solid blue.
/// Returns Err(String) on failure.
#[cfg(target_os = "android")]
pub fn fill_surface_blue(env: &mut JNIEnv, surface: &JObject) -> Result<(), String> {
    // ANativeWindow_fromSurface needs the JNI env pointer (JNIEnv*),
    // NOT the native interface (JNINativeInterface*).
    let window = unsafe {
        ndk_sys::ANativeWindow_fromSurface(
            env.get_raw() as *mut _,
            **surface as jni::sys::jobject,
        )
    };
    if window.is_null() {
        return Err("ANativeWindow_fromSurface returned null".into());
    }

    let mut buf: ndk_sys::ANativeWindow_Buffer = unsafe { std::mem::zeroed() };
    let mut rect = ndk_sys::ARect { left: 0, top: 0, right: 0, bottom: 0 };
    let rc = unsafe { ndk_sys::ANativeWindow_lock(window, &mut buf, &mut rect) };
    if rc != 0 {
        unsafe { ndk_sys::ANativeWindow_release(window); }
        return Err(format!("ANativeWindow_lock failed: {rc}"));
    }

    // Fill blue: format is typically WINDOW_FORMAT_RGBA_8888 or RGBX_8888
    // On most devices, it's BGRA byte order in memory.
    // 0xFF_0000FF = B=0xFF, G=0x00, R=0x00, A=0xFF → opaque blue
    let color: u32 = 0xFF_0000FF;
    let pixel_count = buf.width as usize * buf.height as usize;
    let bits = buf.bits as *mut u32;
    for i in 0..pixel_count {
        unsafe { *bits.add(i) = color; }
    }

    unsafe { ndk_sys::ANativeWindow_unlockAndPost(window); }
    unsafe { ndk_sys::ANativeWindow_release(window); }
    Ok(())
}

#[cfg(not(target_os = "android"))]
pub fn fill_surface_blue(_env: &mut JNIEnv, _surface: &JObject) -> Result<(), String> {
    Ok(())
}

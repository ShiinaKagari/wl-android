/// JNI ↔ Rust type conversion helpers.
/// Thin wrappers over jni crate for common patterns.

use jni::objects::JObject;
use jni::JNIEnv;

/// Get ANativeWindow from a Surface JObject using NDK.
/// This is the key bridge: Kotlin SurfaceView → Rust Vulkan swapchain.
#[allow(dead_code)]
pub fn surface_to_native_window(
    _env: &mut JNIEnv,
    surface: &JObject,
) -> Result<*mut std::ffi::c_void, String> {
    // M6b: ANativeWindow_fromSurface(env, surface) via ndk-sys
    // For now, return a placeholder
    Err("ANativeWindow_fromSurface not yet implemented (M6b)".into())
}

/// Release an ANativeWindow when done.
#[allow(dead_code)]
pub fn release_native_window(_window: *mut std::ffi::c_void) {
    // M6b: ANativeWindow_release(window)
}

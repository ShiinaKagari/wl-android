mod socket;
mod vulkan;

use std::os::fd::AsRawFd;
use std::sync::Mutex;

use jni::objects::JClass;
use jni::sys::{jboolean, jint, JNI_TRUE};
use jni::JNIEnv;

static RENDERER: Mutex<Option<vulkan::VulkanRenderer>> = Mutex::new(None);

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_land_MainActivity_nativeInit(
    _env: JNIEnv,
    _class: JClass,
) -> jboolean {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("land-native"),
    );

    // 连接 socketd
    if let Err(e) = socket::start() {
        log::error!("[land-native] socket start failed: {}", e);
        return 0;
    }

    match vulkan::VulkanRenderer::new() {
        Ok(renderer) => {
            let mut guard = RENDERER.lock().unwrap();
            *guard = Some(renderer);
            log::info!("[land-native] initialized");
            JNI_TRUE
        }
        Err(e) => {
            log::error!("[land-native] vulkan init failed: {}", e);
            0
        }
    }
}

/// C ABI: 由 bridge.cpp 调用，传递 ANativeWindow* 指针。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn land_native_set_surface(
    native_window: *mut std::ffi::c_void,
    width: i32,
    height: i32,
) -> jboolean {
    let mut guard = RENDERER.lock().unwrap();
    let renderer = match *guard {
        Some(ref mut r) => r,
        None => return 0,
    };
    if native_window.is_null() {
        log::error!("[land-native] null native window");
        return 0;
    }
    match renderer.create_android_surface(native_window, width as u32, height as u32) {
        Ok(()) => { log::info!("[land-native] surface created"); JNI_TRUE }
        Err(e) => { log::error!("[land-native] surface creation failed: {}", e); 0 }
    }
}

/// 由 MainActivity 的 Choreographer vsync 回调调用。
/// 渲染最新帧缓存中的 DMA-BUF fd。
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_land_MainActivity_nativeRenderFrame(
    _env: JNIEnv,
    _class: JClass,
) {
    let fd = {
        let mut guard = socket::LATEST_FRAME.lock().unwrap();
        guard.take()
    };
    let meta = {
        let mut guard = socket::FRAME_META.lock().unwrap();
        guard.take()
    };

    let (fd, meta) = match (fd, meta) {
        (Some(f), Some(m)) => (f, m),
        _ => return,
    };

    let renderer = RENDERER.lock().unwrap();
    if let Some(ref renderer) = *renderer {
        if let Err(e) = renderer.import_and_render(fd.as_raw_fd(), meta.width, meta.height) {
            log::error!("[land-native] render failed: {}", e);
        }
    }
    // fd 在此处 drop，自动 close
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_land_MainActivity_nativeDestroy(
    _env: JNIEnv,
    _class: JClass,
) {
    let mut guard = RENDERER.lock().unwrap();
    *guard = None;
    log::info!("[land-native] destroyed");
}

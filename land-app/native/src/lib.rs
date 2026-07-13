mod socket;
mod vulkan;

use std::sync::Mutex;

use jni::objects::JClass;
use jni::sys::{jboolean, jfloat, jint, JNI_TRUE};
use jni::JNIEnv;

use land_common::types::default_socket_path;

static RENDERER: Mutex<Option<vulkan::VulkanRenderer>> = Mutex::new(None);
static SENDER: Mutex<Option<socket::TouchSender>> = Mutex::new(None);

// ── App 生命周期 (JNI regular, @FastNative) ──────────────────

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

    let socket_path = default_socket_path();

    let r = (|| -> Result<(), Box<dyn std::error::Error>> {
        let renderer = vulkan::VulkanRenderer::new()?;
        let sender = socket::TouchSender::connect(&socket_path)?;

        let mut r_guard = RENDERER.lock().unwrap();
        *r_guard = Some(renderer);

        let mut s_guard = SENDER.lock().unwrap();
        *s_guard = Some(sender);

        Ok(())
    })();

    match r {
        Ok(()) => {
            log::info!("[land-native] initialized");
            JNI_TRUE
        }
        Err(e) => {
            log::error!("[land-native] init failed: {}", e);
            0
        }
    }
}

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_land_MainActivity_nativeRenderFrame(
    _env: JNIEnv,
    _class: JClass,
    fd: jint,
    width: jint,
    height: jint,
) {
    let renderer = RENDERER.lock().unwrap();
    if let Some(ref renderer) = *renderer {
        if let Err(e) = renderer.import_and_render(fd, width as u32, height as u32) {
            log::error!("[land-native] render failed: {}", e);
        }
    }
}

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_land_MainActivity_nativeDestroy(
    _env: JNIEnv,
    _class: JClass,
) {
    let mut r_guard = RENDERER.lock().unwrap();
    *r_guard = None;
    let mut s_guard = SENDER.lock().unwrap();
    *s_guard = None;
    log::info!("[land-native] destroyed");
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn land_native_set_surface(
    native_window: *mut std::ffi::c_void,
    width: i32,
    height: i32,
) -> jboolean {
    let mut renderer = RENDERER.lock().unwrap();
    let renderer = match *renderer {
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

// ── 触摸手势 (@CriticalNative: 无 env/class 参数) ────────────

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_land_TouchForwarder_nativeTouchDown(
    id: jint, x: jfloat, y: jfloat,
) {
    let mut sender = SENDER.lock().unwrap();
    if let Some(ref mut s) = *sender {
        if let Err(e) = s.send_touch_down(id, x, y) {
            log::error!("[land] touch_down failed: {}", e);
        }
    }
}

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_land_TouchForwarder_nativeTouchMove(
    id: jint, x: jfloat, y: jfloat,
) {
    let mut sender = SENDER.lock().unwrap();
    if let Some(ref mut s) = *sender {
        if let Err(e) = s.send_touch_move(id, x, y) {
            log::error!("[land] touch_move failed: {}", e);
        }
    }
}

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_land_TouchForwarder_nativeTouchUp(
    id: jint, x: jfloat, y: jfloat,
) {
    let mut sender = SENDER.lock().unwrap();
    if let Some(ref mut s) = *sender {
        if let Err(e) = s.send_touch_up(id, x, y) {
            log::error!("[land] touch_up failed: {}", e);
        }
    }
}

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_land_TouchForwarder_nativePinch(
    scale: jfloat,
) {
    let mut sender = SENDER.lock().unwrap();
    if let Some(ref mut s) = *sender {
        if let Err(e) = s.send_pinch(scale) {
            log::error!("[land] pinch failed: {}", e);
        }
    }
}

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_land_TouchForwarder_nativePinchEnd() {
    let mut sender = SENDER.lock().unwrap();
    if let Some(ref mut s) = *sender {
        if let Err(e) = s.send_pinch_end() {
            log::error!("[land] pinch_end failed: {}", e);
        }
    }
}

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_land_TouchForwarder_nativeScroll(
    dx: jfloat, dy: jfloat,
) {
    let mut sender = SENDER.lock().unwrap();
    if let Some(ref mut s) = *sender {
        if let Err(e) = s.send_scroll(dx, dy) {
            log::error!("[land] scroll failed: {}", e);
        }
    }
}

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_land_TouchForwarder_nativeScrollEnd() {
    let mut sender = SENDER.lock().unwrap();
    if let Some(ref mut s) = *sender {
        if let Err(e) = s.send_scroll_end() {
            log::error!("[land] scroll_end failed: {}", e);
        }
    }
}

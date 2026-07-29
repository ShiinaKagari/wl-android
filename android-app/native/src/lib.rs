mod session;
mod render;
mod ahb;
mod jni_bridge;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;

use jni::objects::{JClass, JString, JObject};
use jni::sys::{jfloat, jint, jlong, jobject};
use jni::JNIEnv;

use crate::session::AppSession;
use crate::render::RenderState;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AppState {
    Init = 0,
    Handshake = 1,
    Active = 2,
    Error = 3,
}

#[derive(Debug, Clone)]
pub struct FrameData {
    pub serial: u64,
    pub buffer_id: u32,
    pub width: u32,
    pub height: u32,
}

type Handle = i64;

struct Inner {
    session: Option<AppSession>,
    render: RenderState,
    state: AppState,
    frame_queue: VecDeque<FrameData>,
}

type StateRef = Arc<Mutex<Inner>>;

static STATE_MAP: std::sync::LazyLock<Mutex<Vec<(Handle, StateRef)>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

static NEXT_ID: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);

fn register(state: StateRef) -> Handle {
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    STATE_MAP.lock().unwrap().push((id, state));
    id
}

fn find(handle: Handle) -> Option<StateRef> {
    STATE_MAP.lock().unwrap().iter()
        .find(|(id, _)| *id == handle)
        .map(|(_, s)| s.clone())
}

fn remove(handle: Handle) {
    STATE_MAP.lock().unwrap().retain(|(id, _)| *id != handle);
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub unsafe extern "system" fn JNI_OnLoad(_vm: jni::JavaVM, _: *mut std::ffi::c_void) -> jni::sys::jint {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("land-native"),
    );
    log::info!("JNI_OnLoad: land_native loaded");
    jni::sys::JNI_VERSION_1_6
}

#[unsafe(no_mangle)]
#[unsafe(no_mangle)]
extern "system" fn Java_com_wl_android_NativeBridge_nativeInit(
    mut env: JNIEnv,
    _class: JClass,
    socket_path: JString,
) -> jlong {
    let path: String = match env.get_string(&socket_path) {
        Ok(s) => s.into(),
        Err(_) => return -1,
    };
    log::info!("nativeInit: connecting to {path}");

    let (session, read_stream) = match AppSession::connect(&path) {
        Ok(pair) => pair,
        Err(e) => {
            log::error!("nativeInit: connect failed: {e}");
            let inner = Arc::new(Mutex::new(Inner {
                session: None, render: RenderState::new(),
                state: AppState::Error, frame_queue: VecDeque::new(),
            }));
            return register(inner);
        }
    };

    let state = Arc::new(Mutex::new(Inner {
        session: Some(session),
        render: RenderState::new(),
        state: AppState::Init,
        frame_queue: VecDeque::new(),
    }));

    let handle = register(state.clone());

    let state_clone = state.clone();
    thread::spawn(move || {
        let write_clone = {
            let inner = state_clone.lock().unwrap();
            let ws = inner.session.as_ref().unwrap().write_stream.as_ref();
            ws.try_clone().expect("clone write stream")
        };
        state_clone.lock().unwrap().state = AppState::Handshake;

        let _ = AppSession::run_loop(read_stream, write_clone, move |serial, buffer_id, width, height| {
            if let Ok(mut inner) = state_clone.lock() {
                inner.state = AppState::Active;
                inner.frame_queue.push_back(FrameData { serial, buffer_id, width, height });
            }
        });
    });

    log::info!("nativeInit: connected, handle={handle}");
    handle
}

#[unsafe(no_mangle)]
extern "system" fn Java_com_wl_android_NativeBridge_nativeSetSurface(
    _env: jni::sys::JNIEnv,
    _class: jni::sys::jclass,
    handle: jlong,
    surface: jni::sys::jobject,
) {
    log::info!("nativeSetSurface handle={handle} surface={}", !surface.is_null());
}

#[unsafe(no_mangle)]
extern "system" fn Java_com_wl_android_NativeBridge_nativeOnConfig(
    _env: JNIEnv, _class: JClass, handle: jlong,
    w: jint, h: jint, refresh_millihz: jint, dpi: jint,
) {
    if let Some(state) = find(handle) {
        let mut inner = state.lock().unwrap();
        if let Some(ref mut session) = inner.session {
            let _ = session.send_config(w as u32, h as u32, refresh_millihz as u32, dpi as u32);
        }
    }
}

#[unsafe(no_mangle)]
#[unsafe(no_mangle)]
extern "system" fn Java_com_wl_android_NativeBridge_nativeOnTouch(
    _env: JNIEnv, _class: JClass, handle: jlong,
    touch_id: jint, x: jfloat, y: jfloat, phase: jint, time_ms: jint,
) {
    if let Some(state) = find(handle) {
        let mut inner = state.lock().unwrap();
        if let Some(ref mut session) = inner.session {
            let msg = wl_android_common::proto::TouchMessage::new(
                touch_id, x, y, phase as u32, time_ms as u32,
            );
            let _ = session.send_message(&wl_android_common::proto::Message::Touch(msg));
        }
    }
}

#[unsafe(no_mangle)]
#[unsafe(no_mangle)]
extern "system" fn Java_com_wl_android_NativeBridge_nativeGetState(
    _env: JNIEnv, _class: JClass, handle: jlong,
) -> jint {
    find(handle)
        .map(|s| s.lock().unwrap().state as jint)
        .unwrap_or(AppState::Error as jint)
}

#[unsafe(no_mangle)]
#[unsafe(no_mangle)]
extern "system" fn Java_com_wl_android_NativeBridge_nativeGetSocketFd(
    _env: JNIEnv, _class: JClass, handle: jlong,
) -> jint {
    find(handle)
        .and_then(|s| {
            let inner = s.lock().unwrap();
            inner.session.as_ref().map(|s| s.socket_fd())
        })
        .unwrap_or(-1)
}

#[unsafe(no_mangle)]
#[unsafe(no_mangle)]
extern "system" fn Java_com_wl_android_NativeBridge_nativeDestroy(
    _env: JNIEnv, _class: JClass, handle: jlong,
) {
    log::info!("nativeDestroy handle={handle}");
    remove(handle);
}

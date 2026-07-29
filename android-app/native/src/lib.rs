mod session;
mod render;
mod ahb;
mod jni_bridge;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
use jni::sys::{jfloat, jint, jlong, jobject};
use jni::JNIEnv;

use crate::session::AppSession;
use crate::render::RenderState;

// ── Global state ──

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AppState {
    Init = 0,
    Handshake = 1,
    Active = 2,
    Error = 3,
}

struct State {
    session: Option<AppSession>,
    render: RenderState,
    state: AppState,
}

static mut INSTANCES: Option<HashMap<i64, State>> = None;
static mut NEXT_ID: i64 = 1;

fn instances() -> &'static mut HashMap<i64, State> {
    unsafe {
        if INSTANCES.is_none() {
            INSTANCES = Some(HashMap::new());
        }
        INSTANCES.as_mut().unwrap()
    }
}

fn next_id() -> i64 {
    unsafe {
        let id = NEXT_ID;
        NEXT_ID += 1;
        id
    }
}

// ── JNI functions ──

#[no_mangle]
pub extern "system" fn Java_com_wl_android_NativeBridge_nativeInit(
    mut env: JNIEnv,
    _class: JClass,
    socket_path: JString,
) -> jlong {
    let path: String = env.get_string(&socket_path).unwrap().into();
    let session = match AppSession::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            let id = next_id();
            instances().insert(id, State {
                session: None,
                render: RenderState::new(),
                state: AppState::Error,
            });
            return id;
        }
    };

    let id = next_id();
    instances().insert(id, State {
        session: Some(session),
        render: RenderState::new(),
        state: AppState::Init,
    });
    id
}

#[no_mangle]
pub extern "system" fn Java_com_wl_android_NativeBridge_nativeSetSurface(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    surface: jobject,
) {
    // TODO: ANativeWindow_fromSurface for Vulkan swapchain
}

#[no_mangle]
pub extern "system" fn Java_com_wl_android_NativeBridge_nativeOnConfig(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    w: jint,
    h: jint,
    refresh_millihz: jint,
    dpi: jint,
) {
    if let Some(state) = instances().get_mut(&handle) {
        if let Some(ref mut session) = state.session {
            let _ = session.send_config(w as u32, h as u32, refresh_millihz as u32, dpi as u32);
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_wl_android_NativeBridge_nativeOnTouch(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    touch_id: jint,
    x: jfloat,
    y: jfloat,
    phase: jint,
    time_ms: jint,
) {
    if let Some(state) = instances().get_mut(&handle) {
        if let Some(ref mut session) = state.session {
            let msg = wl_android_common::proto::TouchMessage::new(
                touch_id, x, y, phase as u32, time_ms as u32,
            );
            let _ = session.send_message(&wl_android_common::proto::Message::Touch(msg));
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_wl_android_NativeBridge_nativeGetState(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jint {
    instances()
        .get(&handle)
        .map(|s| s.state as jint)
        .unwrap_or(AppState::Error as jint)
}

#[no_mangle]
pub extern "system" fn Java_com_wl_android_NativeBridge_nativeGetSocketFd(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jint {
    instances()
        .get(&handle)
        .and_then(|s| s.session.as_ref())
        .map(|s| s.socket_fd())
        .unwrap_or(-1)
}

#[no_mangle]
pub extern "system" fn Java_com_wl_android_NativeBridge_nativeDestroy(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    instances().remove(&handle);
}

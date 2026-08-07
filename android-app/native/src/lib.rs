mod session;
mod jni_bridge;

use std::collections::VecDeque;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;

use jni::objects::{JClass, JString};
use jni::sys::{jfloat, jint, jlong, jobject};
use jni::JNIEnv;

use crate::session::AppSession;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AppState {
    Init = 0,
    Active = 2,
    /// CONN-STATE: the recv thread's run_loop failed (server died / socket
    /// error). Java's status overlay maps this to "Disconnected"; while the
    /// reconnect loop is retrying it stays Disconnected ("Reconnection").
    Disconnected = 4,
}

#[derive(Debug)]
pub struct FrameData {
    pub width: u32,
    pub height: u32,
    /// RENDER-DECOUPLE: pixel fd for the SHM path. The recv thread enqueues
    /// the fd (ownership transfer) instead of rendering inline; the dedicated
    /// render thread mmaps it, copies into the window, drops it, and replies
    /// with a RELEASE so the server can reuse the memfd.
    pub pixel_fd: Option<OwnedFd>,
}

type Handle = i64;

struct Inner {
    session: Option<AppSession>,
    state: AppState,
    /// RENDER-DECOUPLE: recv thread pushes frames here; the render thread
    /// pops the NEWEST one (latest-wins) so at most two frames are ever in
    /// flight — safe against the server's 3-buffer FrameCache rotation.
    /// Wakeup goes through the process-global FRAME_CV (not a field, so the
    /// render thread's wait never borrows through the mutex it parks on).
    frame_queue: VecDeque<FrameData>,
    /// PERF-13: dedicated write path for input (Touch/Key) — a clone of the
    /// session's write stream, guarded by its own mutex so UI-thread input
    /// never contends with the recv thread's Inner lock (frame bookkeeping).
    input_write: Mutex<Option<Arc<std::sync::Mutex<UnixStream>>>>,
    /// Stateless protocol: CONF is a plain event, so no config caching is
    /// needed for a handshake — but nativeOnConfig can still fire before the
    /// session exists (connection retrying), so keep the latest to send on
    /// connect.
    pending_config: Option<(u32, u32, u32, u32, u32)>,
    /// CONN-LOOP: set by nativeDestroy to stop the reconnect thread (it owns
    /// an Arc of Inner and would otherwise retry forever).
    stopped: std::sync::atomic::AtomicBool,
}

type StateRef = Arc<Mutex<Inner>>;

static STATE_MAP: std::sync::LazyLock<Mutex<Vec<(Handle, StateRef)>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

static NEXT_ID: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);

/// CONN-STATE: process-global JavaVM saved in JNI_OnLoad, used by the recv
/// thread (a plain Rust thread) to attach and call the Java status listener.
static JAVA_VM: std::sync::OnceLock<Arc<jni::JavaVM>> = std::sync::OnceLock::new();

/// CONN-STATE: process-global Java StatusListener (global ref). Kept OUTSIDE
/// Inner so notify_status can be called while the caller already holds an
/// Inner lock (no self-deadlock). Single-activity app: one listener suffices.
static STATUS_LISTENER: Mutex<Option<jni::objects::GlobalRef>> = Mutex::new(None);

/// RENDER-DECOUPLE: global frame-ready condvar (shared by the recv thread's
/// notify and the render thread's park). Kept OUT of Inner so the render
/// thread's wait does not borrow through the same mutex it parks on.
static FRAME_CV: std::sync::Condvar = std::sync::Condvar::new();

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
pub unsafe extern "system" fn JNI_OnLoad(vm: jni::JavaVM, _: *mut std::ffi::c_void) -> jni::sys::jint {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("land-native"),
    );
    log::info!("JNI_OnLoad: land_native loaded");
    let _ = JAVA_VM.set(Arc::new(vm));
    jni::sys::JNI_VERSION_1_6
}

/// CONN-STATE: notify the Java StatusListener (if registered) of a state
/// change. Runs on the recv thread (a plain Rust thread) — attaches to the
/// VM per call and posts onStateChanged to the listener.
fn notify_status(state: AppState) {
    let Some(vm) = JAVA_VM.get().cloned() else { return };
    let listener = STATUS_LISTENER.lock().unwrap().clone();
    let Some(listener) = listener else { return };
    let mut env = match vm.attach_current_thread() {
        Ok(e) => e,
        Err(e) => {
            log::error!("notify_status: attach failed: {e}");
            return;
        }
    };
    let state_int = state as i32;
    if let Err(e) = env.call_method(
        &listener,
        "onStateChanged",
        "(I)V",
        &[jni::objects::JValue::Int(state_int)],
    ) {
        log::error!("notify_status: call failed: {e}");
    }
}

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

    let state = Arc::new(Mutex::new(Inner {
        session: None,
        state: AppState::Init,
        frame_queue: VecDeque::new(),
        input_write: Mutex::new(None),
        pending_config: None,
        stopped: std::sync::atomic::AtomicBool::new(false),
    }));

    let handle = register(state.clone());

    // CONN-LOOP: the recv thread owns the connection lifecycle. run_loop
    // returns on any socket error (server restart, network blip); instead of
    // dying and freezing the display, it reconnects and re-enters the loop.
    // The write stream is re-published to `input_write` on every connect so
    // UI-thread Touch/Key sends always target the live socket.
    let state_clone = state.clone();
    thread::spawn(move || {
        log::info!("recv_thread: started (connection loop)");
        // CONN-STATE: true once a session has been established. A later
        // disconnect shows "Disconnected"; never having connected (initial
        // server-down) shows "Reconnection" while retrying.
        let mut had_session = false;
        loop {
            if state_clone.lock().unwrap().stopped.load(std::sync::atomic::Ordering::Relaxed) {
                log::info!("recv_thread: stopped by destroy");
                break;
            }
            match AppSession::connect(&path) {
                Ok((session, read_stream)) => {
                    had_session = true;
                    // Publish session + input write stream atomically.
                    {
                        let mut inner = state_clone.lock().unwrap();
                        inner.session = Some(session);
                        if let Some(ws) = inner.session.as_ref().map(|s| s.write_stream.clone()) {
                            inner.input_write.lock().unwrap().replace(ws);
                        }
                        // Stateless protocol: no handshake — run_loop's
                        // on_connected callback marks Active right away.
                    }
                    let on_frame_state = state_clone.clone();
                    let on_connected_state = state_clone.clone();
                    let result = AppSession::run_loop(
                        read_stream,
                        move || {
                            // Stateless protocol: the session is live the
                            // moment it connects (no handshake to wait for).
                            if let Ok(mut inner) = on_connected_state.lock() {
                                inner.state = AppState::Active;
                                // Flush any config the UI thread cached while
                                // the connection was retrying.
                                if let Some((w, h, r, d, m)) = inner.pending_config.take()
                                    && let Some(ref mut session) = inner.session
                                {
                                    let _ = session.send_config(w, h, r, d, m);
                                }
                                notify_status(AppState::Active);
                            }
                        },
                        move |width, height, pixel_fd: Option<OwnedFd>| {
                            log::debug!("FRAME: {width}x{height} pixels={}", pixel_fd.is_some());
                            // RENDER-DECOUPLE: the pixel fd is enqueued
                            // (latest-wins in the render thread); the recv
                            // thread never touches ANativeWindow.
                            if let Ok(mut inner) = on_frame_state.lock() {
                                inner.state = AppState::Active;
                                inner.frame_queue.push_back(FrameData { width, height, pixel_fd });
                                crate::FRAME_CV.notify_one();
                            }
                        },
                        move |w, h, _r, _dpi, _mode| {
                            log::info!("config_update: {w}x{h}");
                            crate::jni_bridge::set_render_size(w, h);
                        },
                    );
                    match result {
                        Ok(()) => log::info!("run_loop exited cleanly"),
                        Err(ref e) => log::error!("run_loop failed: {e}"),
                    }
                    // CONN-STATE: run_loop ended (clean or error) — the server
                    // side is gone. Mark Disconnected so the Java overlay shows
                    // "Disconnected" (and "Reconnection" during the retries).
                    {
                        let mut inner = state_clone.lock().unwrap();
                        inner.state = AppState::Disconnected;
                        notify_status(AppState::Disconnected);
                    }
                }
                Err(e) => {
                    log::error!("connect failed: {e}");
                    // CONN-STATE: reconnect retry. If we HAD a session, the
                    // overlay keeps showing "Disconnected" (a session was
                    // lost); never having connected shows "Reconnection".
                    let new_state = if had_session { AppState::Disconnected } else { AppState::Init };
                    let mut inner = state_clone.lock().unwrap();
                    inner.state = new_state;
                    notify_status(new_state);
                }
            }
            // Tear down the dead session so JNI sends drop their messages,
            // then retry after a short backoff.
            //
            // CONN-STATE: after a run_loop failure the state was already set
            // to Disconnected above; after a connect failure it was set by
            // had_session. Do not overwrite either here.
            {
                let mut inner = state_clone.lock().unwrap();
                inner.session = None;
                inner.input_write.lock().unwrap().take();
            }
            log::info!("reconnecting in 1s...");
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }
    });

    // RENDER-DECOUPLE: dedicated render thread. Parks on the condvar until a
    // frame is queued, then renders the NEWEST queued frame (latest-wins:
    // older queued fds are dropped — at most 2 frames in flight, safe against
    // the server's 3-buffer FrameCache rotation). The recv thread never
    // touches ANativeWindow, so a slow lock/copy can't stall frame intake.
    {
        let render_state = state.clone();
        thread::spawn(move || {
            log::info!("render_thread: started");
            // CONN-STATE: cleared on Disconnected so the screen is blanked
            // once, not repeatedly while parked on the condvar.
            let mut blanked_while_disconnected = false;
            // DIAG-BLACKSCREEN: rendered-frame counter for the heartbeat log.
            let mut rendered_frames: u64 = 0;
            // Per-fd mmap cache: KWin's buffer pool rotates through a few
            // fds; mapping each once avoids 32MB mmap+munmap per frame.
            let mut mmap_cache = crate::jni_bridge::FdMmapCache::new();
            loop {
                let frame = loop {
                    let mut guard = render_state.lock().unwrap();
                    if guard.stopped.load(std::sync::atomic::Ordering::Relaxed) {
                        log::info!("render_thread: stopped by destroy");
                        return;
                    }
                    match guard.frame_queue.pop_back() {
                        Some(f) => {
                            // latest-wins: every older queued frame is
                            // dropped NOW — each one still owes the server a
                            // RELEASE (its memfd is in flight until the App
                            // says it is done with it). One RELEASE per
                            // dropped/consumed fd keeps the server's FIFO
                            // ownership count exact; leaking them would
                            // wedge one buffer in flight forever.
                            let mut dropped = 0usize;
                            while let Some(old) = guard.frame_queue.pop_front() {
                                if old.pixel_fd.is_some() {
                                    dropped += 1;
                                }
                            }
                            // Send the releases OUTSIDE the Inner lock: clone
                            // the shared write stream, then write.
                            let ws = guard.input_write.lock().unwrap().clone();
                            if dropped > 0 {
                                if let Some(ref ws) = ws {
                                    let data = wl_android_common::proto::encode(
                                        &wl_android_common::proto::Message::Release(
                                            wl_android_common::proto::ReleaseMessage::new(),
                                        ),
                                    );
                                    let mut buf = Vec::with_capacity(4 + data.len());
                                    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
                                    buf.extend_from_slice(&data);
                                    let mut s = ws.lock().unwrap();
                                    for _ in 0..dropped {
                                        let _ = s.write(&buf);
                                    }
                                }
                            }
                            blanked_while_disconnected = false;
                            break f;
                        }
                        None => {
                            // CONN-STATE: no frame pending and the session is
                            // gone → blank the screen once (disconnect shows
                            // black instead of a stale frozen frame). The
                            // Java overlay adds the Disconnected/Reconnection
                            // text on top.
                            let disconnected = guard.state != AppState::Active;
                            if disconnected && !blanked_while_disconnected {
                                blanked_while_disconnected = true;
                                crate::jni_bridge::blank_screen();
                            }
                            // Timed park: re-check state even without a new
                            // frame (a reconnect that never delivers a frame
                            // would otherwise leave a stale blank on resume).
                            guard = crate::FRAME_CV
                                .wait_timeout(guard, std::time::Duration::from_millis(100))
                                .unwrap()
                                .0;
                        }
                    }
                };
                if let Some(fd) = frame.pixel_fd {
                    let _ = crate::jni_bridge::render_frame_fd_cached(
                        frame.width, frame.height, &fd, &mut mmap_cache,
                    );
                    // DIAG-BLACKSCREEN: heartbeat — every 120 rendered
                    // frames log a line so logcat shows whether frames are
                    // arriving and being rendered (vs. stuck on blank).
                    rendered_frames += 1;
                    if rendered_frames % 120 == 0 {
                        log::info!(
                            "render heartbeat: {rendered_frames} frames rendered ({}x{}), queue_depth={}",
                            frame.width,
                            frame.height,
                            render_state.lock().unwrap().frame_queue.len(),
                        );
                    }
                    // Stateless: the frame's pixel fd has been consumed —
                    // release the memfd so the server can reuse it (one
                    // RELEASE per consumed/dropped fd keeps the server's
                    // FIFO ownership count exact).
                    drop(fd);
                    // Send via the shared input_write stream — no Inner lock
                    // needed (it is an independent Arc<Mutex<UnixStream>>),
                    // so this never contends with the recv thread's frame
                    // bookkeeping.
                    let ws = {
                        let inner = render_state.lock().unwrap();
                        inner.input_write.lock().unwrap().clone()
                    };
                    if let Some(ws) = ws {
                        let data = wl_android_common::proto::encode(
                            &wl_android_common::proto::Message::Release(
                                wl_android_common::proto::ReleaseMessage::new(),
                            ),
                        );
                        let mut buf = Vec::with_capacity(4 + data.len());
                        buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
                        buf.extend_from_slice(&data);
                        let _ = ws.lock().unwrap().write(&buf);
                    }
                }
            }
        });
    }
    handle
}

#[unsafe(no_mangle)]
extern "system" fn Java_com_wl_android_NativeBridge_nativeSetSurface(
    env: jni::sys::JNIEnv,
    _class: jni::sys::jclass,
    handle: jlong,
    surface: jni::sys::jobject,
) {
    log::info!("nativeSetSurface handle={handle} surface={}", !surface.is_null());
    crate::jni_bridge::set_surface(env as *mut std::ffi::c_void, surface);
    if surface.is_null() {
        log::info!("nativeSetSurface: surface cleared — CPU path stays dormant until re-set");
        return;
    }
    // SHM-only stateless protocol: frames are pixel fds, rendered via
    // ANativeWindow_lock in the render thread. No Vulkan swapchain, no caps.
    log::info!("nativeSetSurface: surface set — CPU presentation path");
}

#[unsafe(no_mangle)]
extern "system" fn Java_com_wl_android_NativeBridge_nativeOnConfig(
    _env: JNIEnv, _class: JClass, handle: jlong,
    w: jint, h: jint, refresh_millihz: jint, dpi: jint, frame_mode: jint,
) {
    if let Some(state) = find(handle) {
        let mut inner = state.lock().unwrap();
        let cfg = (w as u32, h as u32, refresh_millihz as u32, dpi as u32, frame_mode as u32);
        // Stateless protocol: CONF is a plain event. Send it when a session
        // exists; otherwise cache it and flush on the next connect (the
        // connection may still be retrying).
        if let Some(ref mut session) = inner.session {
            let _ = session.send_config(cfg.0, cfg.1, cfg.2, cfg.3, cfg.4);
        } else {
            inner.pending_config = Some(cfg);
        }
    }
}

#[unsafe(no_mangle)]
extern "system" fn Java_com_wl_android_NativeBridge_nativeSetRenderSize(
    _env: JNIEnv, _class: JClass, _handle: jlong,
    w: jint, h: jint,
) {
    crate::jni_bridge::set_render_size(w as u32, h as u32);
}

#[unsafe(no_mangle)]
extern "system" fn Java_com_wl_android_NativeBridge_nativeOnTouch(
    _env: JNIEnv, _class: JClass, handle: jlong,
    touch_id: jint, x: jfloat, y: jfloat, phase: jint, time_ms: jint,
) {
    let Some(state) = find(handle) else {
        log::warn!("nativeOnTouch: unknown handle {handle}");
        return;
    };
    let msg = wl_android_common::proto::TouchMessage::new(
        touch_id, x, y, phase as u32, time_ms as u32,
    );
    send_input_message(&state, &wl_android_common::proto::Message::Touch(msg));
}

#[unsafe(no_mangle)]
extern "system" fn Java_com_wl_android_NativeBridge_nativeOnKey(
    _env: JNIEnv, _class: JClass, handle: jlong,
    keycode: jint, state: jint, time_ms: jint,
) {
    let Some(state_ref) = find(handle) else { return };
    let msg = wl_android_common::proto::KeyMessage::new(
        keycode as u32, state as u32, time_ms as u32,
    );
    send_input_message(&state_ref, &wl_android_common::proto::Message::Key(msg));
}

/// PERF-13: send an input message over the dedicated input write stream —
/// only the tiny `input_write` mutex is taken (a clone of the session's
/// write end), never the Inner lock that the recv thread holds for frame
/// handling. Drops the message when no session is connected.
///
/// The underlying UnixStream is the SAME mutex-guarded write end that
/// send_config and the render thread's RELEASE writes use — all App→server
/// sends serialize through it, so message bytes can never interleave on the
/// SOCK_STREAM.
fn send_input_message(state: &StateRef, msg: &wl_android_common::proto::Message) {
    let Ok(inner) = state.lock() else {
        log::warn!("send_input_message: state lock failed");
        return;
    };
    let Some(ws) = inner.input_write.lock().unwrap().clone() else {
        log::warn!("send_input_message: no input_write stream");
        return;
    };
    let data = wl_android_common::proto::encode(msg);
    let mut buf = Vec::with_capacity(4 + data.len());
    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
    buf.extend_from_slice(&data);
    match ws.lock().unwrap().write(&buf) {
        Ok(n) => log::debug!("send_input_message: wrote {n} bytes"),
        Err(e) => log::warn!("send_input_message: write failed: {e}"),
    }
}

#[unsafe(no_mangle)]
extern "system" fn Java_com_wl_android_NativeBridge_nativeSetStatusListener(
    env: JNIEnv, _class: JClass, handle: jlong, listener: jobject,
) {
    let _ = handle;
    if listener.is_null() {
        // Clear: drop the global ref so the recv thread stops calling back.
        STATUS_LISTENER.lock().unwrap().take();
        return;
    }
    // CONN-STATE: keep a global ref to the Java StatusListener instance so
    // the recv thread can invoke onStateChanged without polling. The old
    // listener (if any) is released.
    match unsafe { env.new_global_ref(jni::objects::JObject::from_raw(listener)) } {
        Ok(g) => {
            log::info!("nativeSetStatusListener: registered");
            *STATUS_LISTENER.lock().unwrap() = Some(g);
        }
        Err(e) => log::error!("nativeSetStatusListener: new_global_ref failed: {e}"),
    }
}

#[unsafe(no_mangle)]
extern "system" fn Java_com_wl_android_NativeBridge_nativeDestroy(
    _env: JNIEnv, _class: JClass, handle: jlong,
) {
    log::info!("nativeDestroy handle={handle}");
    if let Some(state) = find(handle) {
        state.lock().unwrap().stopped.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    remove(handle);
}

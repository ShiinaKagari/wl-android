mod session;
mod jni_bridge;

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

type Handle = i64;

// ── SNAPSHOT-POOL ──────────────────────────────────────────────────────
// FRONTEND-TEARING FIX: the snapshot (copy out of KWin's shared mapping)
// must happen on the RECV thread the instant a frame arrives — the render
// thread can be delayed 0-21ms by ANativeWindow_lock waits, by which time
// KWin (1-frame delayed release) has rewritten the buffer. State protocol:
//   FREE    recv may write (mmap copy)
//   READY   recv finished; render may take
//   READING render took; recv must NOT touch — data is stable while the
//           render thread locks ANativeWindow and copies
// data is UnsafeCell: access is serialized by the state protocol
// (single writer in FREE, single reader in READING), never concurrent.
const SNAP_FREE: u8 = 0;
const SNAP_READY: u8 = 1;
const SNAP_READING: u8 = 2;

struct SnapshotBuf {
    data: std::cell::UnsafeCell<Vec<u8>>,
    w: std::sync::atomic::AtomicU32,
    h: std::sync::atomic::AtomicU32,
    state: std::sync::atomic::AtomicU8,
}

// SAFETY: the state protocol guarantees no concurrent data access (recv
// writes only in FREE, render reads only in READING).
unsafe impl Sync for SnapshotBuf {}

struct SnapshotPool {
    bufs: [SnapshotBuf; 2],
}

impl SnapshotPool {
    fn new() -> Self {
        let mk = |state: u8| SnapshotBuf {
            data: std::cell::UnsafeCell::new(Vec::new()),
            w: std::sync::atomic::AtomicU32::new(0),
            h: std::sync::atomic::AtomicU32::new(0),
            state: std::sync::atomic::AtomicU8::new(state),
        };
        Self { bufs: [mk(SNAP_FREE), mk(SNAP_FREE)] }
    }

    /// recv thread: snapshot into the first FREE buffer via `f`. Returns
    /// false when both buffers are busy (latest-wins drop — the caller
    /// still owes a RELEASE for the dropped frame) or f failed.
    fn write_with(&self, w: u32, h: u32, f: impl FnOnce(&mut Vec<u8>) -> bool) -> bool {
        for b in &self.bufs {
            if b.state.load(std::sync::atomic::Ordering::Relaxed) == SNAP_FREE {
                // SAFETY: FREE guarantees recv is the only writer.
                let data = unsafe { &mut *b.data.get() };
                if !f(data) {
                    return false;
                }
                b.w.store(w, std::sync::atomic::Ordering::Relaxed);
                b.h.store(h, std::sync::atomic::Ordering::Relaxed);
                b.state.store(SNAP_READY, std::sync::atomic::Ordering::Release);
                return true;
            }
        }
        false
    }

    /// render thread: take the newest READY buffer (mark READING). Returns
    /// (idx, w, h) or None.
    fn take_ready(&self) -> Option<(usize, u32, u32)> {
        for (i, b) in self.bufs.iter().enumerate().rev() {
            if b.state.load(std::sync::atomic::Ordering::Relaxed) == SNAP_READY {
                b.state.store(SNAP_READING, std::sync::atomic::Ordering::Relaxed);
                return Some((
                    i,
                    b.w.load(std::sync::atomic::Ordering::Relaxed),
                    b.h.load(std::sync::atomic::Ordering::Relaxed),
                ));
            }
        }
        None
    }

    /// render thread: data of a READING buffer — stable (recv skips it).
    fn data(&self, idx: usize) -> &[u8] {
        // SAFETY: idx was taken via take_ready (READING); recv never writes
        // a READING buffer.
        unsafe { &(*self.bufs[idx].data.get()) }
    }

    /// render thread: finished displaying — release the buffer to recv.
    fn done(&self, idx: usize) {
        self.bufs[idx].state.store(SNAP_FREE, std::sync::atomic::Ordering::Release);
    }
}

static SNAPSHOT_POOL: std::sync::OnceLock<SnapshotPool> = std::sync::OnceLock::new();
// ── end SNAPSHOT-POOL ─────────────────────────────────────────────────

struct Inner {
    session: Option<AppSession>,
    state: AppState,

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

    // INIT-IDEMPOTENT: if a session already exists for this process (e.g.
    // the Activity was recreated and onCreate ran nativeInit again), reuse it
    // instead of spawning a SECOND recv thread. Two recv threads each open
    // their own land connection; the server's replace-on-new-connect (C-01)
    // then kills one connection per accept, so the killed thread reconnects,
    // which kills the other — an infinite reconnect loop (socket closed every
    // second). Returning the existing handle keeps the original recv thread
    // (and its live connection) untouched.
    if let Some((handle, _)) = STATE_MAP.lock().unwrap().first().cloned() {
        log::info!("nativeInit: reusing existing state handle={handle}");
        return handle;
    }

    let state = Arc::new(Mutex::new(Inner {
        session: None,
        state: AppState::Init,

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
                            // SNAPSHOT-POOL: copy out of the shared mapping
                            // NOW (recv thread, no ANativeWindow waits) —
                            // the render thread's later display copy can
                            // never race KWin's rewrite. RELEASE follows
                            // immediately.
                            thread_local! {
                                static RECV_MMAP_CACHE: std::cell::RefCell<crate::jni_bridge::FdMmapCache> =
                                    std::cell::RefCell::new(crate::jni_bridge::FdMmapCache::new());
                            }
                            if let Some(fd) = pixel_fd {
                                let wrote = RECV_MMAP_CACHE.with(|cache| {
                                    let mut cache = cache.borrow_mut();
                                    SNAPSHOT_POOL.get_or_init(SnapshotPool::new).write_with(
                                        width, height,
                                        |out| {
                                            match crate::jni_bridge::snapshot_frame_into(
                                                width, height, &fd, &mut cache, out,
                                            ) {
                                                Ok(()) => true,
                                                Err(e) => {
                                                    log::warn!("snapshot failed: {e}");
                                                    false
                                                }
                                            }
                                        },
                                    )
                                });
                                if let Ok(mut inner) = on_frame_state.lock() {
                                    inner.state = AppState::Active;
                                    crate::FRAME_CV.notify_one();
                                    // RELEASE: snapshot consumed the shared
                                    // mapping; KWin may rewrite freely.
                                    if let Some(ref ws) = inner.input_write.lock().unwrap().clone() {
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
                                    if !wrote {
                                        log::debug!("snapshot pool busy — dropped frame");
                                    }
                                }
                            }
                        },
                        move |w, h, _r, _dpi, _mode| {
                            log::info!("config_update: {w}x{h}");
                            crate::jni_bridge::set_render_size(w, h);
                            if crate::jni_bridge::gpu_ready() {
                                crate::jni_bridge::gpu_resize(w, h);
                            }
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
            loop {
                // SNAPSHOT-POOL: take the newest READY snapshot (marked
                // READING — recv cannot touch it). Inner lock is held only
                // for the stop/blank checks, never across the copy.
                let taken = {
                    let mut guard = render_state.lock().unwrap();
                    if guard.stopped.load(std::sync::atomic::Ordering::Relaxed) {
                        log::info!("render_thread: stopped by destroy");
                        return;
                    }
                    let pool = SNAPSHOT_POOL.get_or_init(SnapshotPool::new);
                    match pool.take_ready() {
                        Some(t) => {
                            blanked_while_disconnected = false;
                            Some(t)
                        }
                        None => {
                            let disconnected = guard.state != AppState::Active;
                            if disconnected && !blanked_while_disconnected {
                                blanked_while_disconnected = true;
                                if crate::jni_bridge::gpu_ready() {
                                    crate::jni_bridge::gpu_blank();
                                } else {
                                    crate::jni_bridge::blank_screen();
                                }
                            }
                            guard = crate::FRAME_CV
                                .wait_timeout(guard, std::time::Duration::from_millis(100))
                                .unwrap()
                                .0;
                            None
                        }
                    }
                };
                let Some((idx, w, h)) = taken else { continue };
                // Display the stable snapshot (READING protects it from recv
                // while we wait on ANativeWindow_lock — no Inner lock held).
                let data = SNAPSHOT_POOL.get_or_init(SnapshotPool::new).data(idx);
                // GPU-BLIT: set up the EGL renderer once (first frame, window
                // available). Falls back to CPU if EGL is unavailable.
                {
                    use std::sync::atomic::{AtomicBool, Ordering};
                    static GPU_INIT_ATTEMPTED: AtomicBool = AtomicBool::new(false);
                    if !crate::jni_bridge::gpu_ready()
                        && !GPU_INIT_ATTEMPTED.swap(true, Ordering::Relaxed)
                    {
                        let win = crate::jni_bridge::current_window();
                        if !win.is_null() {
                            crate::jni_bridge::gpu_setup(win, w, h);
                        }
                    }
                }
                // GPU-BLIT: present via PBO-pipelined blit; CPU memcpy path is
                // the fallback (GPU not ready, or a frame failed to present).
                let presented = if crate::jni_bridge::gpu_ready() {
                    crate::jni_bridge::gpu_present(data, w, h)
                } else {
                    false
                };
                if !presented {
                    let _ = crate::jni_bridge::render_frame(w, h, data);
                }
                // Release the buffer back to recv.
                SNAPSHOT_POOL.get_or_init(SnapshotPool::new).done(idx);
                rendered_frames += 1;
                if rendered_frames % 120 == 0 {
                    log::info!(
                        "render heartbeat: {rendered_frames} frames rendered ({}x{})",
                        w,
                        h,
                    );
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

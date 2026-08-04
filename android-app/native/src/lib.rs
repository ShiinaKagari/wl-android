mod session;
mod render;
mod ahb;
mod jni_bridge;

use std::collections::VecDeque;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;

use jni::objects::{JClass, JString};
use jni::sys::{jfloat, jint, jlong};
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
    /// Server capabilities from the handshake HELO (SERVER_CAP_*). The
    /// recv thread writes it on HELO; nativeSetSurface reads it to decide
    /// whether to init the Vulkan swapchain (SERVER_CAP_SHM ⇒ pure CPU path).
    server_caps: Arc<std::sync::atomic::AtomicU32>,
    /// App-side bring-up gate (V-33): consecutive `present` failures. When a
    /// streak reaches [`PRESENT_FAIL_GATE`] the frame loop logs a clear
    /// "UBWC import failure suspected" message (the actual pixel read-back is
    /// the server doctor's `import_ubwc_test`, lane 32).
    consecutive_present_failures: u32,
    /// PERF-13: dedicated write path for input (Touch/Key) — a clone of the
    /// session's write stream, guarded by its own mutex so UI-thread input
    /// never contends with the recv thread's Inner lock (frame bookkeeping).
    input_write: Mutex<Option<Arc<UnixStream>>>,
}

/// How many consecutive present failures trip the UBWC-suspicion gate.
const PRESENT_FAIL_GATE: u32 = 3;

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
                server_caps: Arc::new(std::sync::atomic::AtomicU32::new(0)),
                consecutive_present_failures: 0,
                input_write: Mutex::new(None),
            }));
            return register(inner);
        }
    };

    let state = Arc::new(Mutex::new(Inner {
        session: Some(session),
        render: RenderState::new(),
        state: AppState::Init,
        frame_queue: VecDeque::new(),
        server_caps: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        consecutive_present_failures: 0,
        input_write: Mutex::new(None),
    }));

    let handle = register(state.clone());

    // PERF-13: publish the dedicated input write stream — a clone of the
    // session's write end — so UI-thread Touch/Key sends never take the
    // Inner lock that the recv thread holds for frame bookkeeping.
    {
        let inner = state.lock().unwrap();
        if let Some(ws) = inner.session.as_ref().map(|s| s.write_stream.clone()) {
            inner.input_write.lock().unwrap().replace(ws);
        }
    }

    let state_clone = state.clone();
    thread::spawn(move || {
        log::info!("recv_thread: started");
        let write_clone = {
            let inner = state_clone.lock().unwrap();
            let ws = inner.session.as_ref().unwrap().write_stream.as_ref();
            ws.try_clone().expect("clone write stream")
        };
        state_clone.lock().unwrap().state = AppState::Handshake;

        // Lanes 27/30: AhbSlots are built + registered in nativeSetSurface
        // (the SurfaceView surface arrives AFTER this thread spawns, so run_loop
        // can't build them here). run_loop receives an empty list: its startup
        // registration warns and skips — the real TBUF+handle registration is
        // performed by `register_swapchain_slots` once the surface arrives.
        let caps = {
            let inner = state_clone.lock().unwrap();
            inner.server_caps.clone()
        };
        let result = AppSession::run_loop(read_stream, write_clone, Vec::new(), caps, move |serial, buffer_id, width, height, fence_fd: Option<OwnedFd>, pixel_data: &[u8]| {
            log::info!("FRAME: serial={serial} {width}x{height} buf={buffer_id} data={}B fence={}", pixel_data.len(), if fence_fd.is_some() { "yes" } else { "no" });
            if let Some(fence) = fence_fd {
                // Fence path (F-12, lane 30): the server already blitted into
                // swapchain slot buffer_id and shipped the sync_file fence
                // (owned by this callback). Present under the Inner lock:
                // import the fence as a wait semaphore → vkQueuePresentKHR →
                // destroy the temp semaphore; fall back to a CPU poll of the
                // fence when SYNC_FD import is unavailable or fails.
                present_fence_frame(&state_clone, serial, buffer_id, &fence);
            } else {
                // Legacy SHM frame (pre-blit server): CPU copy fallback. Only
                // reachable when the swapchain path isn't driving the frames.
                #[allow(deprecated)]
                let _ = crate::jni_bridge::render_frame(serial, width, height, pixel_data);
            }
            if let Ok(mut inner) = state_clone.lock() {
                inner.state = AppState::Active;
                inner.frame_queue.push_back(FrameData { serial, buffer_id, width, height });
            }
        });
        if let Err(ref e) = result {
            log::error!("recv_thread: run_loop failed: {e}");
        }
    });
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
        // Surface removed. Keep the swapchain as-is: a later set_surface may
        // re-arm it. Surface re-creation (M5, rotation) is out of lane 30's
        // scope — the existing slots stay registered (possibly stale).
        log::info!("nativeSetSurface: surface cleared (swapchain left as-is)");
        return;
    }
    let Some(state) = find(handle) else {
        log::error!("nativeSetSurface: unknown handle {handle}");
        return;
    };
    let mut inner = state.lock().unwrap();

    // LAND_MODE=shm fallback: the server advertises SERVER_CAP_SHM, frames
    // carry pixel fds (no fence), and the App presents via the CPU path. Do
    // NOT init the Vulkan swapchain — ANativeWindow_lock + a live Vulkan
    // swapchain on the same window conflict (lock returns -22).
    let caps = inner.server_caps.load(std::sync::atomic::Ordering::Relaxed);
    if caps & wl_android_common::proto::SERVER_CAP_SHM != 0 {
        log::info!("nativeSetSurface: SERVER_CAP_SHM — CPU presentation path (no Vulkan swapchain)");
        return;
    }

    if inner.render.initialized {
        log::warn!(
            "nativeSetSurface: swapchain already initialized — surface re-creation (M5) not handled in this lane; existing slots stay registered (may be stale)"
        );
        return;
    }
    let window = match crate::jni_bridge::window_ptr() {
        Some(w) => w,
        None => {
            log::warn!("nativeSetSurface: set_surface stored no window — CPU fallback stays active");
            return;
        }
    };

    // 1. Build the swapchain on the SurfaceView's ANativeWindow.
    if let Err(e) = inner.render.init(window) {
        log::error!("nativeSetSurface: render.init failed: {e} — CPU fallback path stays active");
        return;
    }
    log::info!(
        "nativeSetSurface: swapchain initialized — format={:?} extent={}x{} images={}",
        inner.render.image_format(),
        inner.render.extent().width,
        inner.render.extent().height,
        inner.render.images().len(),
    );

    // 2. App-side host-driver SYNC_FD runtime assertion (V-33): the server's
    // blit fence is a sync_file; presenting on it requires importing it as a
    // VkSemaphore (VK_KHR_external_semaphore_fd). render.rs probed the
    // extension during init; surface the verdict here. A missing extension
    // degrades fence frames to the wait_sync_fd CPU poll (logged per frame).
    log::info!(
        "nativeSetSurface: SYNC_FD semaphore import {} ({} fence frames)",
        if inner.render.semaphore_fd_supported() { "SUPPORTED" } else { "UNSUPPORTED" },
        if inner.render.semaphore_fd_supported() { "import → present" } else { "wait_sync_fd CPU-poll fallback" },
    );

    // 3. Slot registration (P-13): one AhbSlot per swapchain image, each
    // shipped as TBUF + AHB native_handle. Done HERE — not in run_loop, which
    // spawned before the surface existed and already warned+skipped with an
    // empty list (see the ordering analysis in `register_swapchain_slots`).
    if let Err(e) = register_swapchain_slots(&mut inner) {
        log::error!("nativeSetSurface: slot registration failed: {e} — blit will stall; server gates frames on {} TBUFs", wl_android_common::proto::SLOT_COUNT);
        return;
    }
    log::info!("nativeSetSurface: swapchain slots registered — blit mode armed");
}

/// Present a fence frame (F-12/F-29, lane 30): the server blitted into
/// swapchain slot `buffer_id` and shipped the sync_file fence. Runs on the
/// recv thread, inside the Inner lock.
///
/// Primary: `import_sync_fd_as_semaphore` → `present(slot, [sem])` →
/// `destroy_semaphore`. Fallback when the import is unavailable or fails:
/// CPU-poll the fence (`wait_sync_fd`, 1s) then `present(slot, [])` — the
/// blit is known complete, so no GPU wait is needed. A fence frame that can
/// neither be imported nor waited on is dropped (the slot stays occupied; the
/// server re-arms it via BRDY only after a successful present path).
///
/// Bring-up gate (V-33): a streak of [`PRESENT_FAIL_GATE`] present failures
/// Route-1 present: on a fence frame (server blitted into the App's LINEAR
/// AHB slot), import the sync_file fence, GPU-blit the AHB into the acquired
/// swapchain image, then present. The blit waits on the fence so we never
/// sample the AHB before the server finished writing it.
fn present_fence_frame(state: &StateRef, serial: u64, buffer_id: u32, fence: &OwnedFd) {
    let mut inner = match state.lock() {
        Ok(i) => i,
        Err(e) => {
            log::error!("present: inner lock poisoned: {e}");
            return;
        }
    };
    if !inner.render.initialized {
        log::warn!(
            "present: fence frame for slot {buffer_id} (serial={serial}) but swapchain uninitialized — dropped"
        );
        return;
    }
    // The swapchain image to blit into. The server's buffer_id == slot number;
    // acquire a free swapchain image (u64::MAX = block until available).
    let swapchain_index = match inner.render.acquire_next_image(u64::MAX, None, None) {
        Ok(i) => i,
        Err(e) => {
            inner.consecutive_present_failures += 1;
            log::warn!("present: acquire for slot {buffer_id} failed: {e}");
            return;
        }
    };

    let sem = match inner.render.import_sync_fd_as_semaphore(fence) {
        Ok(s) => Some(s),
        Err(import_err) => {
            log::warn!(
                "present: SYNC_FD import failed for slot {buffer_id}: {import_err} — fallback: wait_sync_fd"
            );
            match inner.render.wait_sync_fd(fence, 1000) {
                Ok(true) => None,
                Ok(false) => {
                    log::warn!("present: fence wait timed out (1s) for slot {buffer_id} (serial={serial}) — frame dropped");
                    return;
                }
                Err(e) => {
                    log::warn!("present: wait_sync_fd failed for slot {buffer_id}: {e}");
                    return;
                }
            }
        }
    };

    let present_result = inner.render.blit_ahb_to_swapchain(buffer_id, swapchain_index, sem);
    if let Some(s) = sem {
        inner.render.destroy_semaphore(s);
    }
    match present_result {
        Ok(()) => {
            log::info!("present: slot={buffer_id} (serial={serial}, fence-waited) — AHB blit + swapchain present OK");
            inner.consecutive_present_failures = 0;
        }
        Err(e) => {
            inner.consecutive_present_failures += 1;
            log::warn!("present: slot={buffer_id} failed: {e}");
            if inner.consecutive_present_failures == PRESENT_FAIL_GATE {
                log::error!(
                    "PRESENT FAILURE SUSPECTED: {n} consecutive present failures on slot {buffer_id} (serial={serial})",
                    n = inner.consecutive_present_failures,
                );
            }
        }
    }
}

/// P-13/P-14 slot registration (route 1), run on the JNI thread in
/// `nativeSetSurface`: allocate SLOT_COUNT standalone LINEAR AHardwareBuffers
/// ([`AhbSlot::allocate`] — CPU_READ_OFTEN forces LINEAR so the server's
/// turnip can import the dma-buf without crashing), import each into the
/// renderer's AHB image table, then for each slot send the TBUF message
/// followed IMMEDIATELY by the AHB native_handle
/// ([`AhbSlot::send_registration`]) on the session socket — order is
/// load-bearing (the server decodes TBUF and treats the very next bytes as
/// the handle, mirroring `AppSession::send_tbuf_then_handle`).
///
/// Thread-safety (single-writer analysis): these sends run under the Inner
/// lock — the same lock all other JNI sends (CONF/touch/key) already use —
/// so they cannot interleave with each other. The run_loop thread writes its
/// own `wr` clone (FACK/BRDY), a pre-existing pattern from the mmap era; two
/// write ends of one socket could in principle interleave a length-prefixed
/// message mid-frame, but the window is closed here: the server gates blit
/// frames on [`wl_android_common::proto::SLOT_COUNT`] TBUFs, so no FACK/BRDY
/// traffic can exist before these TBUFs flush. Full single-writer discipline
/// (one lock for ALL sends) is a follow-up (M7).
fn register_swapchain_slots(inner: &mut Inner) -> Result<u32, String> {
    let extent = inner.render.extent();
    let slots = crate::ahb::allocate_slots(extent.width, extent.height)?;
    for slot in &slots {
        let ahb = slot.raw_ahb_ptr().ok_or_else(|| {
            format!("slot {} has no AHB pointer", slot.slot)
        })?;
        inner
            .render
            .import_ahb_image(slot.slot, ahb, extent.width, extent.height)
            .map_err(|e| format!("import AHB slot {}: {e}", slot.slot))?;
    }
    if slots.len() != wl_android_common::proto::SLOT_COUNT {
        log::warn!(
            "allocated {} slots but server gates blit on {} TBUFs — blit may stall",
            slots.len(),
            wl_android_common::proto::SLOT_COUNT,
        );
    }
    let session = inner.session.as_mut().ok_or("no session — connect failed")?;
    let sock_fd = session.socket_fd();
    for slot in &slots {
        session
            .send_message(&slot.to_tbuf_message())
            .map_err(|e| format!("TBUF send for slot {} failed: {e}", slot.slot))?;
        slot.send_registration(sock_fd)
            .map_err(|e| format!("slot {} native_handle send failed: {e}", slot.slot))?;
        log::info!(
            "slot registered: slot={} {}x{} fmt={:#x} stride={}",
            slot.slot, slot.width, slot.height, slot.format, slot.stride_bytes,
        );
    }
    Ok(slots.len() as u32)
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
extern "system" fn Java_com_wl_android_NativeBridge_nativeOnTouch(
    _env: JNIEnv, _class: JClass, handle: jlong,
    touch_id: jint, x: jfloat, y: jfloat, phase: jint, time_ms: jint,
) {
    let Some(state) = find(handle) else { return };
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
/// only the tiny `input_write` mutex is taken (a clone of the session's write
/// end), never the Inner lock that the recv thread holds for frame handling.
/// Drops the message when no session is connected (session: None or stream
/// absent) — same observable behavior as the previous Inner-locked path.
///
/// The whole message (length prefix + payload) is written in ONE write()
/// syscall: on SOCK_STREAM a single write is atomic, so this never interleaves
/// with the recv thread's FACK writes on its own stream clone.
fn send_input_message(state: &StateRef, msg: &wl_android_common::proto::Message) {
    let Ok(mut inner) = state.lock() else { return };
    let Some(ws) = inner.input_write.lock().unwrap().clone() else { return };
    let data = wl_android_common::proto::encode(msg);
    let mut buf = Vec::with_capacity(4 + data.len());
    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
    buf.extend_from_slice(&data);
    // Single write() call: on SOCK_STREAM the kernel treats one write as an
    // atomic unit, so the message can never interleave with the recv thread's
    // FACK writes on its own stream clone. The message is ~40B and the socket
    // buffer is ≥64KiB, so the write completes in one call in practice.
    let _ = ws.as_ref().write(&buf);
}

#[unsafe(no_mangle)]
extern "system" fn Java_com_wl_android_NativeBridge_nativeGetState(
    _env: JNIEnv, _class: JClass, handle: jlong,
) -> jint {
    find(handle)
        .map(|s| s.lock().unwrap().state as jint)
        .unwrap_or(AppState::Error as jint)
}

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
extern "system" fn Java_com_wl_android_NativeBridge_nativeDestroy(
    _env: JNIEnv, _class: JClass, handle: jlong,
) {
    log::info!("nativeDestroy handle={handle}");
    remove(handle);
}

use std::sync::Arc;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode};
use smithay::wayland::socket::ListeningSocketSource;
use tracing::{error, info, warn};

use crate::app_link::{AppSession, SessionMode};
use crate::state::WlState;
use crate::transport::Transport;
use wl_android_common::proto;
use wl_android_common::proto::Message;

mod ahb_handle;
mod app_link;
mod blit;
mod comp;
mod doctor;
mod frame_cache;
mod frame_router;
mod state;
mod touch;
mod transport;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("doctor") => {
            doctor::run();
            Ok(())
        }
        Some("run") | None => run_server(),
        Some(cmd) => {
            eprintln!("unknown command: {cmd}");
            eprintln!("usage: wl-android [run|doctor]");
            std::process::exit(1);
        }
    }
}

fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    let wayland_display =
        std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "land-0".into());
    let xdg_runtime =
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let wayland_socket_path = format!("{xdg_runtime}/{wayland_display}");

    // Default land socket in user space (not /run)
    let land_socket_path =
        std::env::var("LAND_SOCKET").unwrap_or_else(|_| {
            format!("{xdg_runtime}/wl-android/land.sock")
        });

    // Clean stale sockets from previous runs.
    //
    // The land socket is deployed inside a DIRECTORY bind mount
    // (host /data/local/tmp/wl-android ↔ container /run/wl-android): the
    // directory is the mount point, and the socket FILE inside it is a
    // regular entry — unlink here removes the stale socket from the shared
    // directory (both sides), then create_listener binds a fresh one. This
    // is safe BECAUSE the file is not itself a mount point (a single-file
    // bind would make it one: EBUSY on unlink, EADDRINUSE on bind).
    for stray in &[
        &wayland_socket_path,
        &format!("{wayland_socket_path}.lock"),
        &land_socket_path,
    ] {
        std::fs::remove_file(stray).ok();
    }

    // Ensure land socket parent dir exists
    if let Some(parent) = std::path::Path::new(&land_socket_path).parent() {
        std::fs::create_dir_all(parent).ok();
    }

    info!(wayland = %wayland_socket_path, land = %land_socket_path, "starting wl-android");

    let mut event_loop: EventLoop<WlState> =
        EventLoop::try_new().expect("create event loop");

    let mut state = WlState::new()?;

    // Wayland listening socket
    let wayland_socket = ListeningSocketSource::with_name(&wayland_display)?;
    event_loop
        .handle()
        .insert_source(wayland_socket, move |stream, _, state| {
            let client = match state.display.handle().insert_client(stream, Arc::new(())) {
                Ok(c) => c,
                Err(e) => { error!(err = %e, "failed to insert wayland client"); return; }
            };
            info!(id = ?client.id(), "Wayland client connected");
            // Dispatch immediately to send initial globals to the new client
            state.dispatch_wayland();
        })?;

    info!("listening on wayland socket {wayland_display}");

    // Ensure non-root clients (kagari) can connect
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wayland_socket_path, std::fs::Permissions::from_mode(0o666)).ok();

    // Land socket — store listener in state for idle polling
    match app_link::create_listener(&land_socket_path) {
        Ok(listener) => {
            state.land_listener = Some(listener);
            info!("land socket at {land_socket_path}");
        }
        Err(e) => {
            warn!("land socket not available: {e}");
        }
    }

    let event_handle = event_loop.handle();

    event_loop.run(Some(Duration::from_millis(16)), &mut state, |state| {
        // Dispatch pending Wayland client messages
        state.dispatch_wayland();

        // ── Accept new App connections ──
        let mut connect_actions = Vec::new();
        let mut listener_opt = state.land_listener.take();
        if let Some(ref mut listener) = listener_opt {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        info!("App connected");
                        if let Ok(transport) = Transport::new(stream.try_clone().expect("clone land stream")) {
                            if state.app_session.is_some() {
                                // C-01: replacing the old session — release its slots.
                                state.clear_blit_pipeline_state();
                                state.blit_engine.clear_slots();
                                connect_actions = state.frame_router.handle(
                                    crate::frame_router::RouterEvent::AppLost,
                                );
                                dispatch_router_actions(state, &connect_actions);
                                // The old session's land source is removed by
                                // dropping the source handle below (replace).
                                if let Some(old_token) = state.land_source.take() {
                                    event_handle.remove(old_token);
                                }
                            }
                            state.app_session = Some(AppSession::new(transport));
                            connect_actions = state.frame_router.handle(
                                crate::frame_router::RouterEvent::AppConnected,
                            );

                            // PERF-13: register the land socket fd as an
                            // event-driven source — App input (Touch/Key/Config/
                            // Ack/Ready) wakes the loop immediately, independent
                            // of the 16ms tick and of KWin frame processing.
                            // The callback owns the session drain: it runs
                            // handle_land_input once per readable fd state, and
                            // returns Remove on session teardown so calloop
                            // unregisters the source (the token is dropped too).
                            let fd_clone = stream.try_clone().expect("clone land fd for source");
                            let source = Generic::new(
                                fd_clone,
                                Interest::READ,
                                Mode::Level,
                            );
                            let cb = |_readiness, _fd: &mut _, state: &mut WlState| -> std::io::Result<calloop::PostAction> {
                                let lost = handle_land_input(state);
                                if lost {
                                    // C-02: blit mode — close all slot fds and
                                    // destroy the VkImages.
                                    state.clear_blit_pipeline_state();
                                    state.blit_engine.clear_slots();
                                    let actions = state.frame_router.handle(
                                        crate::frame_router::RouterEvent::AppLost,
                                    );
                                    dispatch_router_actions(state, &actions);
                                    state.app_session = None;
                                    Ok(calloop::PostAction::Remove)
                                } else {
                                    Ok(calloop::PostAction::Continue)
                                }
                            };
                            match event_handle.insert_source(source, cb) {
                                Ok(token) => {
                                    state.land_source = Some(token);
                                    // Handshake is driven by the source's
                                    // readiness (the App's HELO arrives right
                                    // after connect); kick it here too so the
                                    // first message is consumed even if the
                                    // fd event raced ahead of insert_source.
                                    let _ = handle_land_input(state);
                                }
                                Err(e) => {
                                    error!(err = %e, "failed to register land source");
                                }
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        error!(err = %e, "accept error");
                        break;
                    }
                }
            }
        }
        state.land_listener = listener_opt;
        dispatch_router_actions(state, &connect_actions);
    })?;

    // Cleanup on exit
    info!("shutting down, cleaning sockets");
    std::fs::remove_file(&wayland_socket_path).ok();
    std::fs::remove_file(&land_socket_path).ok();

    Ok(())
}

/// Drain one App land-socket message (handshake / slot registration / active
/// input) and apply it to the compositor state. Returns true when the session
/// must be torn down (protocol error or disconnect).
///
/// Event-driven (PERF-13): called from the Generic land source readiness
/// check inside the event loop, so App input is handled as soon as the fd is
/// readable — not on the 16ms poll tick and not behind KWin frame processing.
fn handle_land_input(state: &mut WlState) -> bool {
    let Some(session) = &mut state.app_session else {
        return false;
    };
    match session.mode() {
        SessionMode::Handshake => match session.do_handshake() {
            Ok(true) => {
                info!("handshake complete, mode={:?}", session.mode());
                // 握手完成后，把缓存的当前帧发给新客户端，避免黑屏。
                // H-04: blit waits for SLOT_COUNT TBUFs — only direct mode
                // (Active immediately) replays here; blit replays on activation.
                if session.mode() == SessionMode::Active
                    && let Some(cache) = &state.frame_cache
                    && let Some((fd, seq, cw, ch)) = cache.current_frame()
                {
                    let _ = session.send_frame(
                        seq, 0, state.screen_width, state.screen_height,
                        cw, ch, Some(fd), None,
                    );
                }
                false
            }
            Ok(false) => false,
            Err(e) => {
                warn!(err = %e, "handshake failed");
                true
            }
        },
        SessionMode::SlotRegistration => {
            // Wait for TBUF slot messages (H-04: SLOT_COUNT before frames)
            match session.recv_message(&mut state.blit_engine) {
                Ok(Some(Message::Slot(slot))) => {
                    // Already counted in recv_message. F-14: registration
                    // itself marks the slot ready for the FIRST blit — the
                    // App cannot BRDY a slot it has not yet presented a
                    // frame from, so without this implicit grant the first
                    // frame would deadlock (server waits for BRDY, App
                    // waits for a frame to present before BRDYing).
                    // (Direct field borrow, not handle_brdy: `session` is
                    // still live here, so a &mut self call cannot borrow.)
                    state.brdy_ready.mark_ready(slot.slot);
                    info!(count = session.slot_count(), "slot registered");
                    // Check if all slots are registered
                    if session.slot_count() >= proto::SLOT_COUNT as u32 {
                        info!("all slots registered, activating");
                        session.activate();
                        // H-04: blit is now Active — replay the latest cached
                        // frame so the App is not black until the next commit.
                        if let Some(cache) = &state.frame_cache
                            && let Some((fd, seq, cw, ch)) = cache.current_frame()
                        {
                            let _ = session.send_frame(
                                seq, 0, state.screen_width, state.screen_height,
                                cw, ch, Some(fd), None,
                            );
                        }
                    }
                    false
                }
                Ok(None) => false,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
                Err(e) => {
                    warn!(err = %e, "slot registration read error");
                    true
                }
                _ => false,
            }
        }
        SessionMode::Active => {
            match session.recv_message(&mut state.blit_engine) {
                Ok(Some(msg)) => match msg {
                    wl_android_common::proto::Message::Ack(ack) => {
                        // F-11: freed slots (fences destroyed) before
                        // dispatch — an unblocked frame may immediately
                        // reuse a slot this ack just released.
                        let freed = crate::state::free_slots_on_ack(
                            &mut state.slots_in_use, ack.serial,
                        );
                        for (slot, fence) in freed {
                            state.blit_engine.destroy_fence_handle(fence);
                            tracing::info!(slot, ack = ack.serial, "slot freed by cum-ack");
                        }
                        let actions = state.frame_router.handle(
                            crate::frame_router::RouterEvent::AppAck {
                                serial: ack.serial,
                            },
                        );
                        dispatch_router_actions(state, &actions);
                        false
                    }
                    wl_android_common::proto::Message::Touch(tm) => {
                        state.handle_touch(&tm);
                        false
                    }
                    wl_android_common::proto::Message::Key(km) => {
                        state.handle_key(&km);
                        false
                    }
                    wl_android_common::proto::Message::Config(conf) => {
                        state.apply_config(
                            conf.width, conf.height,
                            conf.refresh_millihz, conf.dpi,
                        );
                        false
                    }
                    wl_android_common::proto::Message::Ready(rdy) => {
                        // F-14: App presented the slot's previous fence
                        // frame and releases it for reuse — the slot
                        // becomes eligible for the next blit.
                        state.handle_brdy(rdy.slot);
                        false
                    }
                    _ => false,
                },
                Ok(None) => false,
                Err(e) => {
                    warn!(err = %e, "session read error");
                    true
                }
            }
        }
    }
}

fn dispatch_router_actions(
    state: &mut WlState,
    actions: &[crate::frame_router::RouterAction],
) {
    use crate::frame_router::RouterAction;
    for action in actions {
        match action {
            RouterAction::EnqueueFrame { buffer_id: bid, serial, .. } => {
                state.blit_and_send_frame(*bid, *serial);
            }
            RouterAction::ReleaseBuffer { .. } => {
                // F-10 noop by design: smithay's compositor emits wl_buffer.release
                // itself when a buffer is replaced (merge_into) or removed (tree.rs).
                tracing::trace!("release buffer");
            }
            RouterAction::FireCallback => {
                if let Some(ref tl) = state.toplevel {
                    let surface = tl.wl_surface();
                    let now_ms = std::time::Instant::now()
                        .duration_since(state.clock_epoch)
                        .as_millis() as u32;
                    smithay::wayland::compositor::with_states(surface, |states| {
                        let mut guard = states.cached_state.get::<smithay::wayland::compositor::SurfaceAttributes>();
                        for cb in guard.current().frame_callbacks.drain(..) {
                            cb.done(now_ms);
                        }
                    });
                }
            }
            RouterAction::Gone { buffer_id } => {
                if let Some(session) = &mut state.app_session {
                    let _ = session.send_gone(*buffer_id);
                }
            }
            _ => {}
        }
    }
}

use std::sync::Arc;
use std::time::Duration;

use calloop::EventLoop;
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

    // Clean stale sockets from previous runs
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

    event_loop.run(Some(Duration::from_millis(16)), &mut state, |state| {
        // Dispatch pending Wayland client messages
        state.dispatch_wayland();

        // ── Accept new App connections ──
        let mut connect_actions = Vec::new();
        if let Some(ref listener) = state.land_listener {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        info!("App connected");
                        if let Ok(transport) = Transport::new(stream) {
                            state.app_session = Some(AppSession::new(transport));
                            connect_actions = state.frame_router.handle(
                                crate::frame_router::RouterEvent::AppConnected,
                            );
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
        dispatch_router_actions(state, &connect_actions);

        // ── Poll app session ──
        let lost = if let Some(session) = &mut state.app_session {
            match session.mode() {
                SessionMode::Handshake => match session.do_handshake() {
                    Ok(true) => {
                        info!("handshake complete, mode={:?}", session.mode());
                        false
                    }
                    Ok(false) => false,
                    Err(e) => {
                        warn!(err = %e, "handshake failed");
                        true
                    }
                },
                SessionMode::SlotRegistration => {
                    // Wait for TBUF slot messages
                    match session.recv_message() {
                        Ok(Some(Message::Slot(_))) => {
                            // Already counted in recv_message
                            info!(count = session.slot_count(), "slot registered");
                            // Check if all slots are registered
                            if session.slot_count() >= proto::SLOT_COUNT as u32 {
                                info!("all slots registered, activating");
                                session.activate();
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
                    match session.recv_message() {
                        Ok(Some(msg)) => match msg {
                            wl_android_common::proto::Message::Ack(ack) => {
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
                            wl_android_common::proto::Message::Config(conf) => {
                                state.apply_config(
                                    conf.width, conf.height,
                                    conf.refresh_millihz, conf.dpi,
                                );
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
        } else {
            false
        };
        if lost {
            let actions = state.frame_router.handle(
                crate::frame_router::RouterEvent::AppLost,
            );
            dispatch_router_actions(state, &actions);
            state.app_session = None;
        }
    })?;

    // Cleanup on exit
    info!("shutting down, cleaning sockets");
    std::fs::remove_file(&wayland_socket_path).ok();
    std::fs::remove_file(&land_socket_path).ok();

    Ok(())
}

fn dispatch_router_actions(
    state: &mut WlState,
    actions: &[crate::frame_router::RouterAction],
) {
    use crate::frame_router::RouterAction;
    for action in actions {
        match action {
            RouterAction::EnqueueFrame { buffer_id: bid, serial, .. } => {
                if let Some(session) = &mut state.app_session {
                    let _ = session.send_frame(*serial, *bid, state.screen_width, state.screen_height);
                }
            }
            RouterAction::ReleaseBuffer { .. } => {
                // wl_buffer.release handled by CompositorState
                tracing::trace!("release buffer");
            }
            RouterAction::FireCallback => {
                // Frame callback dispatched via CompositorState
                tracing::trace!("frame callback");
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

use std::sync::Arc;
use std::time::Duration;

use calloop::EventLoop;
use smithay::wayland::socket::ListeningSocketSource;
use tracing::{error, info, warn};

use crate::app_link::{AppSession, SessionMode};
use crate::state::WlState;
use crate::transport::Transport;

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
    let land_socket_path =
        std::env::var("LAND_SOCKET").unwrap_or_else(|_| "/run/wl-android/land.sock".into());

    let mut event_loop: EventLoop<WlState> =
        EventLoop::try_new().expect("create event loop");

    let mut state = WlState::new()?;

    // Wayland listening socket
    let wayland_socket = ListeningSocketSource::with_name(&wayland_display)?;
    event_loop
        .handle()
        .insert_source(wayland_socket, move |stream, _, state| {
            if let Err(e) = state.display.handle().insert_client(stream, Arc::new(())) {
                error!(err = %e, "failed to insert wayland client");
            }
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
        // ── Accept new App connections ──
        if let Some(ref listener) = state.land_listener {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        info!("App connected");
                        if let Ok(transport) = Transport::new(stream) {
                            state.app_session = Some(AppSession::new(transport));
                            state.frame_router.handle(
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

        // ── Poll app session ──
        let lost = if let Some(session) = &mut state.app_session {
            match session.mode() {
                SessionMode::Handshake => match session.do_handshake() {
                    Ok(true) => { info!("handshake complete"); false }
                    Ok(false) => false,
                    Err(e) => {
                        warn!(err = %e, "handshake failed");
                        true
                    }
                },
                SessionMode::Active => {
                    match session.recv_message() {
                        Ok(Some(msg)) => match msg {
                            wl_android_common::proto::Message::Ack(ack) => {
                                state.frame_router.handle(
                                    crate::frame_router::RouterEvent::AppAck {
                                        serial: ack.serial,
                                    },
                                );
                                false
                            }
                            wl_android_common::proto::Message::Touch(tm) => {
                                state.handle_touch(&tm);
                                false
                            }
                            wl_android_common::proto::Message::Config(conf) => {
                                state.apply_config(
                                    conf.width,
                                    conf.height,
                                    conf.refresh_millihz,
                                    conf.dpi,
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
            state.frame_router.handle(crate::frame_router::RouterEvent::AppLost);
            state.app_session = None;
        }
    })?;

    Ok(())
}

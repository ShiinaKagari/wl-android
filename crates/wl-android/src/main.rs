use std::sync::Arc;

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode};
use smithay::wayland::socket::ListeningSocketSource;
use tracing::{error, info, warn};

use crate::app_link::AppSession;
use crate::state::{WlClientState, WlState};
use crate::transport::Transport;

mod app_link;
mod comp;
mod doctor;
mod frame_mem;
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
    // Default display name: use a NON-standard name (land-0), NOT the
    // standard wayland-0. wl-android is the OUTER compositor in a nested
    // setup: KWin runs on top as the inner compositor and needs wayland-0
    // for ITS OWN socket (Plasma shell connects to KWin's wayland-0).
    // Occupying wayland-0 would make KWin fail with "unable to lock
    // lockfile wayland-0.lock, maybe another compositor is running".
    let wayland_display =
        std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "land-0".into());
    // Default runtime dir: prefer XDG_RUNTIME_DIR; per the XDG Base
    // Directory spec §3, when it is unset applications should fall back to
    // a replacement directory with similar capabilities AND print a
    // warning. We use a private $HOME/.cache/wl-runtime (NOT /tmp — the
    // turnip/Vulkan driver segfaults in BlitEngine::drop when the
    // XDG_RUNTIME_DIR env var is unset or empty; any non-empty value
    // works, including /tmp). The fallback is created with mode 0700 and
    // chown'd to the invoking user to match the spec's ownership/0700
    // requirements.
    let xdg_runtime = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            let dir = format!("{home}/.cache/wl-runtime");
            std::fs::create_dir_all(&dir).ok();
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
            // SAFETY: single-threaded startup; path is owned by us.
            unsafe {
                let _ = libc::chown(
                    std::ffi::CString::new(dir.as_str()).unwrap().as_ptr(),
                    libc::getuid(),
                    libc::getgid(),
                );
            }
            tracing::warn!(
                dir,
                "XDG_RUNTIME_DIR is not set — using private fallback (spec §3 allows this; \
                 prefer launching via a systemd user session so /run/user/<uid> is provided)"
            );
            // The driver checks the env var itself; the path alone is not
            // enough. Export so turnip's teardown path does not segfault.
            // SAFETY: single-threaded at startup, no other env access yet.
            unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
            dir
        }
    };
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
            let client = match state
                .display
                .handle()
                .insert_client(stream, Arc::new(WlClientState::default()))
            {
                Ok(c) => c,
                Err(e) => { error!(err = %e, "failed to insert wayland client"); return; }
            };
            info!(id = ?client.id(), "Wayland client connected");
            // Dispatch immediately to send initial globals to the new client
            state.dispatch_wayland();
        })?;

    info!("listening on wayland socket {wayland_display}");

    // VSYNC-PACING: a steady timer at the output refresh rate (144Hz) that
    // flushes queued frame callbacks / presentation feedbacks — KWin's
    // render loop is driven by this beat. Registered BEFORE the land socket
    // source so vsync ticks are never starved by App traffic.
    let vsync_period = state.vsync_period();
    let vsync_timer = calloop::timer::Timer::from_duration(vsync_period);
    // DYNAMIC-PERIOD: each tick reschedules using the CURRENT refresh
    // rate from CONF — a runtime refresh change (LTPO switch, App re-report)
    // takes effect on the next tick instead of staying stuck at the
    // startup period.
    event_loop
        .handle()
        .insert_source(vsync_timer, |_, _meta, state| {
            state.vsync_tick();
            calloop::timer::TimeoutAction::ToDuration(state.vsync_period())
        })?;
    info!(?vsync_period, "vsync timer started");

    let event_handle = event_loop.handle();

    // Ensure non-root clients (kagari) can connect
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wayland_socket_path, std::fs::Permissions::from_mode(0o666)).ok();

    // Land socket — store listener in state; its fd is registered as an
    // event-driven source below so new App connections are accepted
    // WITHOUT a poll tick (the accept loop lives in that source's callback).
    use std::os::fd::AsRawFd;
    match app_link::create_listener(&land_socket_path) {
        Ok(listener) => {
            let fd = listener.as_raw_fd();
            state.land_listener = Some(listener);
            info!("land socket at {land_socket_path}");
            let accept_source: calloop::generic::Generic<std::os::fd::OwnedFd> = Generic::new(
                unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) },
                Interest::READ,
                Mode::Level,
            );
            let eh = event_handle.clone();
            let cb = move |_readiness, _fd: &mut _, state: &mut WlState| -> std::io::Result<calloop::PostAction> {
                accept_land_connections(state, &eh);
                Ok(calloop::PostAction::Continue)
            };
            if let Err(e) = event_handle.insert_source(accept_source, cb) {
                error!(err = %e, "failed to register land accept source");
            }
        }
        Err(e) => {
            warn!("land socket not available: {e}");
        }
    }

    event_loop.run(None, &mut state, |state| {
        // Dispatch pending Wayland client messages
        state.dispatch_wayland();
    })?;

    // Cleanup on exit
    info!("shutting down, cleaning sockets");
    std::fs::remove_file(&wayland_socket_path).ok();
    std::fs::remove_file(&land_socket_path).ok();

    Ok(())
}


/// Accept any pending App connections on the land listener. Event-driven
/// (called from the land accept source) — a connection wakes the loop and
/// is accepted immediately, with no poll tick required.
fn accept_land_connections(state: &mut WlState, event_handle: &calloop::LoopHandle<WlState>) {
    // ── Accept new App connections ──
    let mut listener_opt = state.land_listener.take();
    if let Some(ref mut listener) = listener_opt {
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    info!("App connected");
                    if let Ok(transport) = Transport::new(stream.try_clone().expect("clone land stream")) {
                        if state.app_session.is_some() {
                            // C-01: replacing the old session. The old
                            // session's land source is removed by dropping the
                            // source handle below (replace).
                            if let Some(old_token) = state.land_source.take() {
                                event_handle.remove(old_token);
                            }
                        }
                        state.app_session = Some(AppSession::new(transport));

                        // PERF-13: register the land socket fd as an
                        // event-driven source — App input (Touch/Key/Config/
                        // Ack) wakes the loop immediately, independent of the
                        // poll cadence and of KWin frame processing. The
                        // callback owns the session drain: it runs
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
                                state.app_session = None;
                                Ok(calloop::PostAction::Remove)
                            } else {
                                Ok(calloop::PostAction::Continue)
                            }
                        };
                        match event_handle.insert_source(source, cb) {
                            Ok(token) => {
                                state.land_source = Some(token);
                                // Stateless protocol: the App is ready the
                                // moment it connects. Push the current
                                // geometry (bucketed DPI) immediately so the
                                // App's render window matches; the first
                                // frame arrives with the next KWin commit.
                                if let Some(session) = &mut state.app_session {
                                    let _ = session.send_config_update(
                                        state.screen_width, state.screen_height,
                                        state.refresh_millihz, WlState::bucket_dpi(state.dpi),
                                        state.frame_mode,
                                    );
                                }
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
}
/// Drain one App land-socket message (stateless: Config / Touch / Key /
/// Release — each is an independent event) and apply it to the compositor
/// state. Returns true when the session must be torn down (protocol error or
/// disconnect).
///
/// Event-driven (PERF-13): called from the Generic land source readiness
/// check inside the event loop, so App input is handled as soon as the fd is
/// readable — not on the 16ms poll tick and not behind KWin frame processing.
fn handle_land_input(state: &mut WlState) -> bool {
    let Some(session) = &mut state.app_session else {
        return false;
    };
    match session.recv_message() {
        Ok(Some(msg)) => match msg {
            // Consumption signal: the App finished reading a frame's fd. KWin
            // buffers are released immediately at commit (ASYNC-RELEASE), so
            // there is nothing to return here — this is purely an ack for
            // logging/accounting.
            wl_android_common::proto::Message::Release(_) => {
                tracing::debug!("App released a frame");
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
                // Stateless config event: apply it and mirror the effective
                // (DPI-bucketed) geometry back via CONFU.
                state.apply_config(
                    conf.width, conf.height,
                    conf.refresh_millihz, conf.dpi,
                    conf.frame_mode,
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

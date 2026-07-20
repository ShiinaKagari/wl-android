use std::os::unix::net::UnixListener;

use smithay::delegate_compositor;
use smithay::delegate_output;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_xdg_shell;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Display;
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
use smithay::wayland::dmabuf::DmabufState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgShellHandler, XdgShellState};
use smithay::wayland::shm::{ShmHandler, ShmState};
use tracing::info;
use wayland_protocols::xdg::shell::server::xdg_toplevel;

use crate::app_link::AppSession;
use crate::blit::BlitEngine;
use crate::frame_router::FrameRouter;
use crate::touch::TouchInjector;
use wl_android_common::proto::TouchMessage;

pub struct WlState {
    pub display: Display<Self>,
    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
    pub dmabuf_state: DmabufState,
    pub xdg_shell_state: XdgShellState,
    #[allow(dead_code)]
    pub output_state: OutputManagerState,
    pub frame_router: FrameRouter,
    #[allow(dead_code)]
    pub blit_engine: BlitEngine,
    pub app_session: Option<AppSession>,
    pub land_listener: Option<UnixListener>,
    pub screen_width: u32,
    pub screen_height: u32,
    pub refresh_millihz: u32,
    pub dpi: u32,
    pub output: Output,
    pub toplevel: Option<ToplevelSurface>,
    pub seat_state: SeatState<Self>,
    pub seat: Seat<Self>,
    pub touch_injector: TouchInjector,
}

impl WlState {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let display = Display::new()?;
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let output_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let frame_router = FrameRouter::new();
        let blit_engine = BlitEngine::new();

        let mut dmabuf_state = DmabufState::new();
        let dmabuf_feedback = crate::comp::dmabuf::build_default_feedback();
        let _dmabuf_global =
            dmabuf_state.create_global_with_default_feedback::<Self>(&dh, &dmabuf_feedback);

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_seat("seat-0");
        let _touch = seat.add_touch();

        let w = 3392;
        let h = 2400;
        let refresh = 144_000;
        let dpi = 289;

        let touch_injector = TouchInjector::new(w, h);

        let output = Output::new(
            "eDP-1".into(),
            PhysicalProperties {
                size: ((w as f64 * 25.4 / dpi as f64) as i32, (h as f64 * 25.4 / dpi as f64) as i32).into(),
                subpixel: Subpixel::Unknown,
                make: "BOE".into(),
                model: "OnePlus Pad 3".into(),
            },
        );
        let mode = Mode { size: (w as i32, h as i32).into(), refresh: refresh as i32 };
        output.add_mode(mode);
        output.set_preferred(mode);
        output.change_current_state(Some(mode), None, None, None);
        let _global = output.create_global::<Self>(&dh);

        Ok(Self {
            display, compositor_state, shm_state, dmabuf_state,
            xdg_shell_state, output_state, frame_router, blit_engine,
            app_session: None, land_listener: None,
            screen_width: w, screen_height: h, refresh_millihz: refresh, dpi,
            output, toplevel: None, seat_state, seat, touch_injector,
        })
    }

    pub fn handle_touch(&mut self, msg: &TouchMessage) {
        let touch_opt = self.seat.get_touch();
        if let Some(touch) = touch_opt {
            let ptr = self as *mut Self;
            unsafe { (*ptr).touch_injector.handle(msg, &touch, &mut *ptr); }
        }
    }

    pub fn apply_config(&mut self, w: u32, h: u32, refresh_millihz: u32, dpi: u32) {
        info!(w, h, refresh = refresh_millihz, dpi, "applying config update");
        let size_changed = self.screen_width != w || self.screen_height != h;
        let _refresh_changed = self.refresh_millihz != refresh_millihz;
        self.screen_width = w;
        self.screen_height = h;
        self.refresh_millihz = refresh_millihz;
        self.dpi = dpi;
        self.touch_injector.set_logical_size(w, h);

        let new_mode = Mode { size: (w as i32, h as i32).into(), refresh: refresh_millihz as i32 };
        self.output.add_mode(new_mode);
        self.output.set_preferred(new_mode);
        self.output.change_current_state(Some(new_mode), None, None, None);

        if size_changed
            && let Some(ref tl) = self.toplevel
        {
            tl.with_pending_state(|state| {
                state.size = Some((w as i32, h as i32).into());
                state.states.set(xdg_toplevel::State::Fullscreen);
            });
            tl.send_configure();
        }
    }
}

// ── Compositor ──

impl CompositorHandler for WlState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        _client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        Box::leak(Box::new(CompositorClientState::default()))
    }

    fn commit(&mut self, _surface: &WlSurface) {
        // Frame callback: dispatched via CompositorState post-commit hooks.
        // Smithay handles wl_callback.done internally through the commit machinery.

        let actions = self.frame_router.handle(
            crate::frame_router::RouterEvent::Commit {
                buffer_id: 0, has_fds: false, serial: 0,
            },
        );
        for action in actions {
            match action {
                crate::frame_router::RouterAction::EnqueueFrame { buffer_id: _, serial } => {
                    if let Some(session) = &mut self.app_session {
                        let _ = session.send_frame(
                            serial, 1, self.screen_width, self.screen_height,
                        );
                    }
                }
                crate::frame_router::RouterAction::FireCallback => {
                    // Real frame callback dispatched, no need to duplicate
                }
                _ => {}
            }
        }
    }
}

delegate_compositor!(WlState);
delegate_shm!(WlState);

impl ShmHandler for WlState {
    fn shm_state(&self) -> &ShmState { &self.shm_state }
}

// ── XDG Shell ──

impl XdgShellHandler for WlState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState { &mut self.xdg_shell_state }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.size = Some((self.screen_width as i32, self.screen_height as i32).into());
            state.states.set(xdg_toplevel::State::Fullscreen);
        });
        surface.send_configure();
        self.toplevel = Some(surface);
    }

    fn new_popup(&mut self, _: smithay::wayland::shell::xdg::PopupSurface, _: smithay::wayland::shell::xdg::PositionerState) {}
    fn grab(&mut self, _: smithay::wayland::shell::xdg::PopupSurface, _: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat, _: smithay::utils::Serial) {}
    fn reposition_request(&mut self, _: smithay::wayland::shell::xdg::PopupSurface, _: smithay::wayland::shell::xdg::PositionerState, _: u32) {}
}

delegate_xdg_shell!(WlState);

// ── Seat ──

impl SeatHandler for WlState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> { &mut self.seat_state }
    fn focus_changed(&mut self, _: &Seat<Self>, _: Option<&WlSurface>) {}
    fn cursor_image(&mut self, _: &Seat<Self>, _: smithay::input::pointer::CursorImageStatus) {}
}

delegate_seat!(WlState);

// ── Output ──

impl smithay::wayland::output::OutputHandler for WlState {}
delegate_output!(WlState);

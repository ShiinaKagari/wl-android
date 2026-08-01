use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;

use smithay::delegate_compositor;
use smithay::delegate_content_type;
use smithay::delegate_fractional_scale;
use smithay::delegate_output;
use smithay::delegate_presentation;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_single_pixel_buffer;
use smithay::delegate_viewporter;
use smithay::delegate_xdg_shell;
use smithay::delegate_alpha_modifier;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Display;
use smithay::wayland::compositor::{self, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes};
use smithay::wayland::dmabuf::DmabufState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgShellHandler, XdgShellState};
use smithay::wayland::shm::{self, ShmHandler, ShmState};
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::content_type::ContentTypeState;
use smithay::wayland::alpha_modifier::AlphaModifierState;
use smithay::wayland::pointer_constraints::{PointerConstraintsHandler, PointerConstraintsState};
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::fractional_scale::FractionalScaleHandler;
use smithay::wayland::presentation::{PresentationState, PresentationFeedbackCachedState};
use smithay::wayland::presentation::Refresh;
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
    pub single_pixel_buffer_state: SinglePixelBufferState,
    pub viewporter_state: ViewporterState,
    pub content_type_state: ContentTypeState,
    pub alpha_modifier_state: AlphaModifierState,
    pub pointer_constraints_state: PointerConstraintsState,
    pub fractional_scale_state: FractionalScaleManagerState,
    pub presentation_state: PresentationState,
    pub xdg_shell_state: XdgShellState,
    #[allow(dead_code)]
    pub output_state: OutputManagerState,
    pub frame_router: FrameRouter,
    pub blit_image_handles: Vec<u64>,
    #[allow(dead_code)]
    pub blit_engine: BlitEngine,
    pub app_session: Option<AppSession>,
    pub land_listener: Option<UnixListener>,
    pub clock_epoch: std::time::Instant,
    pub screen_width: u32,
    pub screen_height: u32,
    pub refresh_millihz: u32,
    pub dpi: u32,
    pub output: Output,
    pub toplevel: Option<ToplevelSurface>,
    pub seat_state: SeatState<Self>,
    pub seat: Seat<Self>,
    pub touch_injector: TouchInjector,
    pub pending_pixel_fds: HashMap<u64, OwnedFd>,
}

impl WlState {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let display = Display::new()?;
        let dh = display.handle();

        let compositor_state = CompositorState::new_v6::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let output_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let frame_router = FrameRouter::new();
        let blit_engine = BlitEngine::new();

        let mut dmabuf_state = DmabufState::new();
        let dmabuf_feedback = crate::comp::dmabuf::build_default_feedback();
        let _dmabuf_global =
            dmabuf_state.create_global_with_default_feedback::<Self>(&dh, &dmabuf_feedback);

        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let content_type_state = ContentTypeState::new::<Self>(&dh);
        let alpha_modifier_state = AlphaModifierState::new::<Self>(&dh);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&dh);
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&dh);
        let presentation_state = PresentationState::new::<Self>(&dh, 1); // CLOCK_MONOTONIC

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "seat-0");
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

        let mut state = Self {
            display, compositor_state, shm_state, dmabuf_state, single_pixel_buffer_state,
            viewporter_state, content_type_state, alpha_modifier_state, pointer_constraints_state,
            fractional_scale_state, presentation_state,
            xdg_shell_state, output_state, frame_router, blit_engine,
            blit_image_handles: Vec::new(),
            app_session: None, land_listener: None,
            clock_epoch: std::time::Instant::now(),
            screen_width: w, screen_height: h, refresh_millihz: refresh, dpi,
            output, toplevel: None, seat_state, seat, touch_injector,
            pending_pixel_fds: HashMap::new(),
        };

        // Initialize Vulkan blit engine (turnip) for dmabuf import
        if let Err(e) = state.blit_engine.init() {
            tracing::warn!(err = %e, "blit engine init failed — dmabuf blit not available");
        }

        Ok(state)
    }

    /// Dispatch pending Wayland client messages. Uses unsafe pointer split
    /// to work around Rust's borrow checker — safe in calloop's single-threaded context.
    pub fn dispatch_wayland(&mut self) {
        let self_ptr: *mut Self = self;
        unsafe {
            let display: &mut Display<Self> = &mut (*self_ptr).display;
            match display.dispatch_clients(&mut *self_ptr) {
                Ok(n) => tracing::debug!(n, "wayland dispatch done"),
                Err(e) => tracing::error!(err = %e, "dispatch error"),
            }
            if let Err(e) = display.flush_clients() {
                tracing::error!(err = %e, "flush error");
            }
        }
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
        // C3: Use thread-local static to avoid Box::leak — one per process is fine
        // since we only have one client (KWin nested) in our use case.
        use std::cell::RefCell;
        thread_local! {
            static STATE: RefCell<CompositorClientState> = RefCell::new(CompositorClientState::default());
        }
        // SAFETY: The returned reference must live as long as 'a.
        // The thread_local lives for the process lifetime, satisfying 'a.
        STATE.with(|s| {
            let ptr = s.as_ptr() as *const CompositorClientState;
            unsafe { &*ptr }
        })
    }

    fn commit(&mut self, surface: &WlSurface) {
        tracing::info!("surface commit");

        let buffer_id = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            surface.hash(&mut h);
            h.finish() as u32
        };
        let has_fds = false;

        let mut pixel_fd: Option<std::os::fd::OwnedFd> = None;
        {
            let extracted: Option<(u32, u32, Vec<u8>)> = 'extract: {
                let parent_data = compositor::with_states(surface, |states| {
                    let mut guard = states.cached_state.get::<SurfaceAttributes>();
                    match &guard.current().buffer {
                        Some(BufferAssignment::NewBuffer(wl_buffer)) => {
                            shm::with_buffer_contents(wl_buffer, |ptr, _pool_len, data| {
                                let stride = data.stride as usize;
                                let height = data.height as usize;
                                if height == 0 || stride == 0 { return None; }
                                let offset = data.offset as usize;
                                let mut vec = vec![0u8; height * stride];
                                for y in 0..height {
                                    let src = unsafe { ptr.add(offset + y * stride) };
                                    let dst = unsafe { vec.as_mut_ptr().add(y * stride) };
                                    unsafe { std::ptr::copy_nonoverlapping(src, dst, stride); }
                                }
                                Some((stride as u32 / 4, height as u32, vec))
                            }).unwrap_or(None)
                        }
                        _ => None,
                    }
                });
                if parent_data.is_some() { break 'extract parent_data; }
                let children = compositor::get_children(surface);
                for child in &children {
                    let child_data = compositor::with_states(child, |states| {
                        let mut guard = states.cached_state.get::<SurfaceAttributes>();
                        match &guard.current().buffer {
                            Some(BufferAssignment::NewBuffer(wl_buffer)) => {
                                shm::with_buffer_contents(wl_buffer, |ptr, _pool_len, data| {
                                    let stride = data.stride as usize;
                                    let height = data.height as usize;
                                    if height == 0 || stride == 0 { return None; }
                                    let offset = data.offset as usize;
                                    let mut vec = vec![0u8; height * stride];
                                    for y in 0..height {
                                        let src = unsafe { ptr.add(offset + y * stride) };
                                        let dst = unsafe { vec.as_mut_ptr().add(y * stride) };
                                        unsafe { std::ptr::copy_nonoverlapping(src, dst, stride); }
                                    }
                                    Some((stride as u32 / 4, height as u32, vec))
                                }).unwrap_or(None)
                            }
                            _ => None,
                        }
                    });
                    if child_data.is_some() { break 'extract child_data; }
                }
                None
            };

            if let Some((_bw, _bh, data)) = extracted {
                let size = data.len();
                match nix::sys::memfd::memfd_create(
                    "wl-frame",
                    nix::sys::memfd::MFdFlags::MFD_CLOEXEC | nix::sys::memfd::MFdFlags::MFD_ALLOW_SEALING,
                ) {
                    Ok(memfd) => {
                        use std::os::fd::AsRawFd;
                        if nix::unistd::ftruncate(&memfd, size as _).is_ok() {
                            let ptr = unsafe {
                                nix::sys::mman::mmap(
                                    None,
                                    NonZeroUsize::new(size).unwrap(),
                                    nix::sys::mman::ProtFlags::PROT_READ | nix::sys::mman::ProtFlags::PROT_WRITE,
                                    nix::sys::mman::MapFlags::MAP_SHARED,
                                    &memfd,
                                    0,
                                )
                            };
                            if let Ok(ptr) = ptr {
                                unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.as_ptr() as *mut u8, size); }
                                unsafe { nix::sys::mman::munmap(ptr, size).ok(); }
                                let raw = unsafe { libc::dup(memfd.as_raw_fd()) };
                                if raw >= 0 {
                                    pixel_fd = Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) });
                                }
                            }
                        }
                    }
                    Err(e) => tracing::warn!(err = %e, "memfd_create failed"),
                }
            }
        }

        let actions = self.frame_router.handle(
            crate::frame_router::RouterEvent::Commit {
                buffer_id,
                has_fds,
                serial: 0,
            },
        );
        let has_enqueue = actions.iter().any(|a| matches!(a, crate::frame_router::RouterAction::EnqueueFrame { .. }));
        for action in actions {
            match action {
                crate::frame_router::RouterAction::EnqueueFrame { buffer_id: bid, serial, .. } => {
                    tracing::info!(serial, bid, "sending frame to App");
                    if let Some(session) = &mut self.app_session {
                        let fd = pixel_fd.take();
                        let _ = session.send_frame(
                            serial, bid, self.screen_width, self.screen_height,
                            self.screen_width, self.screen_height, fd,
                        );
                    }
                }
                crate::frame_router::RouterAction::FireCallback => {}
                _ => {}
            }
        }
        if !has_enqueue {
            if let Some(fd) = pixel_fd.take() {
                self.pending_pixel_fds.clear();
                let serial = self.frame_router.current_serial();
                self.pending_pixel_fds.insert(serial, fd);
            }
        }

        let now_ms = std::time::Instant::now()
            .duration_since(self.clock_epoch)
            .as_millis() as u32;
        let period_ns = 1_000_000_000_000u64 / self.refresh_millihz as u64;
        let refresh = Refresh::Fixed(std::time::Duration::from_nanos(period_ns));
        let seq = self.frame_router.current_serial();
        compositor::with_states(surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            let count = guard.current().frame_callbacks.len();
            for cb in guard.current().frame_callbacks.drain(..) {
                cb.done(now_ms);
            }
            if count > 0 {
                tracing::info!(count, "dispatched frame callbacks");
            }
        });
        compositor::with_states(surface, |states| {
            let mut guard = states.cached_state.get::<PresentationFeedbackCachedState>();
            let count = guard.current().callbacks.len();
            for fb in guard.current().callbacks.drain(..) {
                fb.presented(
                    &self.output,
                    std::time::Instant::now().duration_since(self.clock_epoch),
                    refresh,
                    seq,
                    wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync,
                );
            }
            if count > 0 {
                tracing::info!(count, "dispatched presentation feedbacks");
            }
        });
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
        tracing::info!("new toplevel created");
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

delegate_single_pixel_buffer!(WlState);
delegate_viewporter!(WlState);
delegate_content_type!(WlState);
delegate_alpha_modifier!(WlState);
delegate_fractional_scale!(WlState);

impl FractionalScaleHandler for WlState {}

impl PointerConstraintsHandler for WlState {
    fn new_constraint(&mut self, _surface: &WlSurface, _pointer: &smithay::input::pointer::PointerHandle<Self>) {}
    fn cursor_position_hint(&mut self, _surface: &WlSurface, _pointer: &smithay::input::pointer::PointerHandle<Self>, _pos: smithay::utils::Point<f64, smithay::utils::Logical>) {}
}

use smithay::delegate_pointer_constraints;
delegate_pointer_constraints!(WlState);
delegate_presentation!(WlState);

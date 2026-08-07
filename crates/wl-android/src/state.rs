use std::os::fd::OwnedFd;
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
use smithay::backend::input::{ButtonState, KeyState};
use smithay::input::keyboard::{FilterResult, Keycode, XkbConfig};
use smithay::input::pointer::{ButtonEvent, MotionEvent as PointerMotionEvent};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::utils::{Logical, Point, Serial};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_server::backend::ClientData;
use smithay::reexports::wayland_server::protocol::wl_callback::WlCallback;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource as WlResource;
use smithay::reexports::wayland_server::Display;
use smithay::wayland::compositor::{self, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes};
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
use smithay::wayland::presentation::{PresentationFeedbackCallback, PresentationState, PresentationFeedbackCachedState};
use smithay::wayland::presentation::Refresh;
use tracing::info;
use wayland_protocols::xdg::shell::server::xdg_toplevel;

use crate::app_link::AppSession;
use crate::frame_mem::FrameMem;
use crate::touch::TouchInjector;
use wl_android_common::proto::{TouchMessage, TOUCH_PHASE_DOWN, TOUCH_PHASE_MOVE, TOUCH_PHASE_UP};

#[derive(Debug)]
enum ExtractedFrame {
    /// dmabuf fd forwarded directly (zero copy, KWin GPU rendering). The
    /// App consumes it and replies with RELEASE before KWin may reuse it.
    Dmabuf(u32, u32, OwnedFd),
    /// SHM pixels copied into the single FrameMem buffer (smithay does not
    /// expose the client pool fd, so SHM cannot be forwarded directly).
    Shm(u32, u32, OwnedFd),
}

/// Extract a committed buffer's pixels for the App. Two sources:
///
/// * dmabuf (KWin GPU rendering): forward the plane-0 fd directly — zero
///   copy. Only LINEAR buffers are CPU-readable by the App; compressed
///   (UBWC) buffers are dropped (the App cannot import them).
/// * SHM (KWin software rendering): smithay does not expose the client pool
///   fd, so the pixels are copied into the single FrameMem buffer. KWin
///   repaints the whole buffer per frame, so no damage tracking is needed.
fn extract_frame(
    wl_buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    frame_mem: &mut Option<FrameMem>,
) -> Option<ExtractedFrame> {
    use smithay::backend::allocator::Buffer;
    use std::os::fd::{AsRawFd, FromRawFd};

    // dmabuf path: forward the fd directly (zero copy).
    if let Ok(dmabuf) = smithay::wayland::dmabuf::get_dmabuf(wl_buffer) {
        if dmabuf.has_modifier() {
            tracing::warn!(
                modifier = ?dmabuf.format().modifier,
                "dmabuf with non-linear modifier — App cannot read it, dropping frame"
            );
            return None;
        }
        let w = dmabuf.width();
        let h = dmabuf.height();
        let fd = match dmabuf.handles().next() {
            Some(handle) => {
                // dup so the App's fd is independent of the KWin buffer's
                // lifetime (the wl_buffer may be released once RELEASE
                // arrives, but the App still needs the fd valid).
                let raw = unsafe { libc::dup(handle.as_raw_fd()) };
                if raw >= 0 {
                    Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) })
                } else {
                    None
                }
            }
            None => None,
        };
        return fd.map(|fd| ExtractedFrame::Dmabuf(w, h, fd));
    }

    // SHM path: copy into the single frame buffer.
    let shm_result = shm::with_buffer_contents(wl_buffer, |ptr, _pool_len, data| {
        let stride = data.stride as usize;
        let height = data.height as usize;
        if height == 0 || stride == 0 {
            return None;
        }
        let offset = data.offset as usize;
        let w = (stride as u32) / 4;
        if frame_mem.is_none() {
            match FrameMem::new(w, height as u32) {
                Ok(f) => *frame_mem = Some(f),
                Err(e) => {
                    tracing::error!(err = %e, "FrameMem::new failed");
                    return None;
                }
            }
        }
        let mem = frame_mem.as_mut().unwrap();
        if mem.set_dimensions(w, height as u32).is_err() {
            return None;
        }
        // Copy the full frame: KWin repaints the whole buffer per commit,
        // so per-frame contents are complete (no damage tracking needed).
        let needed = w as usize * height * 4;
        let buf = unsafe { std::slice::from_raw_parts(ptr.add(offset), needed) };
        mem.push(buf, w, height as u32)
    })
    .unwrap_or(None);
    let (sw, sh) = shm::with_buffer_contents(wl_buffer, |_ptr, _len, data| {
        (data.stride as u32 / 4, data.height as u32)
    })
    .unwrap_or((0, 0));
    shm_result.map(|fd| ExtractedFrame::Shm(sw, sh, fd))
}




pub struct WlState {
    pub display: Display<Self>,
    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
    pub xdg_shell_state: XdgShellState,
    pub dmabuf_state: smithay::wayland::dmabuf::DmabufState,
    /// Single SHM frame buffer (fallback path; dmabuf frames bypass this).
    pub frame_mem: Option<FrameMem>,
    pub app_session: Option<AppSession>,
    pub land_listener: Option<UnixListener>,
    /// calloop source for the App session's land socket fd (event-driven
    /// input). Replaced on App reconnect; None when no App is connected.
    pub land_source: Option<calloop::RegistrationToken>,
    pub clock_epoch: std::time::Instant,
    pub screen_width: u32,
    pub screen_height: u32,
    pub refresh_millihz: u32,
    pub dpi: u32,
    /// Frame pacing mode from the App's ConfigMessage: 0 free, 1 vsync-align,
    /// 2 performance, 3 power-save. Drives commit-time throttling (last_send).
    pub frame_mode: u32,
    pub output: Output,
    pub toplevel: Option<ToplevelSurface>,
    pub seat_state: SeatState<Self>,
    pub seat: Seat<Self>,
    pub touch_injector: TouchInjector,
    pub next_serial: u32,
    /// VSYNC-PACING: frame callbacks & presentation feedbacks queued by
    /// commits are NOT dispatched immediately — they are flushed by the
    /// vsync timer at the output's refresh rate (144Hz). KWin's render loop
    /// is driven by these signals; an irregular, event-driven dispatch (one
    /// per commit, whenever that happens to arrive) leaves KWin's vsync
    /// monitor without a trustworthy beat, so it falls back to input-driven
    /// rendering — "plasma needs a tap to continue". A stable beat keeps
    /// KWin rendering at min(144Hz, its own capability).
    pub pending_callbacks: Vec<WlCallback>,
    pub pending_feedbacks: Vec<PresentationFeedbackCallback>,
    pub vsync_seq: u64,
}

/// PERF-11 import-cache bound: a destroyed wl_buffer's entry lingers (the
/// comp/dmabuf buffer_destroyed lane cannot reach this map — documented gap),
/// so an arbitrary entry is evicted past the bound instead of leaking GPU memory.


impl WlState {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let display = Display::new()?;
        let dh = display.handle();

        let compositor_state = CompositorState::new_v6::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let _output_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);

        // KWin GPU rendering: advertise zwp_linux_dmabuf_v1 so Mesa's EGL
        // wayland platform can allocate GPU buffers (KWin's EGL backend
        // requires it; without it KWin falls back to software rendering).
        let mut dmabuf_state = smithay::wayland::dmabuf::DmabufState::new();
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
        // Global registration only — the state is owned by the display's
        // global infrastructure, the values are intentionally dropped.
        let _ = (single_pixel_buffer_state, viewporter_state, content_type_state,
                 alpha_modifier_state, pointer_constraints_state,
                 fractional_scale_state, presentation_state);

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "seat-0");
        let _touch = seat.add_touch();
        let _pointer = seat.add_pointer();
        // Explicit US-qwerty keymap (smithay compiles it at add_keyboard time via
        // its internal xkbcommon). Empty fields in XkbConfig::default() would fall
        // back to host env vars / builtin defaults; pinning layout: "us" keeps the
        // keymap deterministic so keycodes are always translatable.
        let _keyboard = seat.add_keyboard(
            XkbConfig { layout: "us", ..Default::default() },
            200,
            25,
        )?;

        let w = 3392;
        let h = 2400;
        let refresh = 144_000;
        let dpi = Self::bucket_dpi(289);

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

        let state = Self {
            display, compositor_state, shm_state, xdg_shell_state, dmabuf_state,
            frame_mem: None,
            app_session: None, land_listener: None, land_source: None,
            clock_epoch: std::time::Instant::now(),
            screen_width: w, screen_height: h, refresh_millihz: refresh, dpi,
            frame_mode: 0,
            output, toplevel: None, seat_state, seat, touch_injector,
            next_serial: 1,
            pending_callbacks: Vec::new(),
            pending_feedbacks: Vec::new(),
            vsync_seq: 1,
        };


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

        let serial = {
            let s = Serial::from(self.next_serial);
            self.next_serial += 1;
            s
        };

        if let Some(ref toplevel) = self.toplevel
            && let Some(pointer) = self.seat.get_pointer()
        {
            let surface = toplevel.wl_surface().clone();
            let surface_pos: Point<f64, Logical> = (0.0, 0.0).into();
            let x = msg.x as f64 * self.screen_width as f64;
            let y = msg.y as f64 * self.screen_height as f64;
            let loc: Point<f64, Logical> = (x, y).into();

            match msg.phase {
                TOUCH_PHASE_DOWN => {
                    pointer.motion(self, Some((surface, surface_pos)), &PointerMotionEvent {
                        location: loc,
                        serial,
                        time: msg.time_ms,
                    });
                    pointer.button(self, &ButtonEvent {
                        serial,
                        time: msg.time_ms,
                        button: 0x110, // BTN_LEFT
                        state: ButtonState::Pressed,
                    });
                    pointer.frame(self);
                }
                TOUCH_PHASE_MOVE => {
                    pointer.motion(self, Some((surface, surface_pos)), &PointerMotionEvent {
                        location: loc,
                        serial,
                        time: msg.time_ms,
                    });
                    pointer.frame(self);
                }
                TOUCH_PHASE_UP => {
                    pointer.button(self, &ButtonEvent {
                        serial,
                        time: msg.time_ms,
                        button: 0x110, // BTN_LEFT
                        state: ButtonState::Released,
                    });
                    pointer.frame(self);
                }
                _ => {}
            }
        }
    }

    /// Map the App protocol key state (1=down, 0=up) to smithay's [`KeyState`].
    ///
    /// Any value other than 1 maps to `Released` so a malformed message can never
    /// inject a phantom press into the seat.
    pub fn key_state_from_u32(state: u32) -> KeyState {
        if state == 1 { KeyState::Pressed } else { KeyState::Released }
    }

    pub fn handle_key(&mut self, msg: &wl_android_common::proto::KeyMessage) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            tracing::debug!("key event dropped: seat has no keyboard");
            return;
        };

        // KEY-FOCUS: ensure the keyboard focus points at the toplevel (KWin's
        // surface) before injecting. smithay's keyboard.input() only delivers
        // to the focused surface; the pointer path sets focus via
        // pointer.motion(Some((surface, ...))) on touch, but the keyboard
        // never got one — so key events were silently dropped (no text input).
        // Setting focus on every key is idempotent (Enter is only re-sent when
        // the focus actually changes).
        if let Some(ref toplevel) = self.toplevel {
            let surface = toplevel.wl_surface().clone();
            let serial = {
                let s = Serial::from(self.next_serial);
                self.next_serial += 1;
                s
            };
            keyboard.set_focus(self, Some(surface), serial);
        }

        let serial = {
            let s = Serial::from(self.next_serial);
            self.next_serial += 1;
            s
        };

        let key_state = Self::key_state_from_u32(msg.state);
        info!(keycode = msg.keycode, state = ?key_state, time_ms = msg.time_ms, "key event from App");

        // Keycode is the raw evdev scancode; smithay expects X-style keycodes
        // (libinput backends do the same +8), and its internal xkbcommon state
        // performs the evdev→keysym translation. Out-of-range keycodes are a
        // safe no-op in xkbcommon, so malformed input cannot panic here.
        keyboard.input(
            self,
            Keycode::new(msg.keycode + 8),
            key_state,
            serial,
            msg.time_ms,
            |_data, _mods, _handle| FilterResult::<()>::Forward,
        );
    }

    pub fn apply_config(&mut self, w: u32, h: u32, refresh_millihz: u32, dpi: u32, frame_mode: u32) {
        info!(w, h, refresh = refresh_millihz, dpi, frame_mode, "applying config update");
        let size_changed = self.screen_width != w || self.screen_height != h;
        self.screen_width = w;
        self.screen_height = h;
        self.refresh_millihz = refresh_millihz;
        self.dpi = dpi;
        self.frame_mode = frame_mode;
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

        // Push the bucketed DPI back so the App's HiDPI scale matches the
        // geometry we advertise to KWin (289 → 288 keeps an integer 2× scale).
        // Stateless protocol: a connected App is always ready for this event.
        if let Some(session) = &mut self.app_session {
            let _ = session.send_config_update(w, h, refresh_millihz, Self::bucket_dpi(dpi), frame_mode);
        }
    }

    /// Map a raw display DPI to the nearest bucket in {96, 120, 144, 160,
    /// 192, 240, 288, 384}. Integer multiples of 96 keep Qt/GTK HiDPI scales
    /// integral (289 DPI on the panel would otherwise produce a fractional
    /// 2.007× scale); the panel reports 289 → 288.
    pub fn bucket_dpi(raw: u32) -> u32 {
        const BUCKETS: [u32; 8] = [96, 120, 144, 160, 192, 240, 288, 384];
        if raw <= BUCKETS[0] {
            return BUCKETS[0];
        }
        let mut best = BUCKETS[0];
        for &b in &BUCKETS {
            if raw >= b {
                best = b;
            } else {
                // The bucket just below raw; decide by distance.
                return if raw - best <= b - raw { best } else { b };
            }
        }
        best
    }

}

// ── Compositor ──

/// PER-CLIENT compositor state, attached to each wayland client at
/// `insert_client` time. The previous implementation used ONE thread-local
/// `CompositorClientState` shared by EVERY client — once plasmashell /
/// Xwayland connected as a second client, its commits polluted KWin's
/// transaction queue and frame callbacks: KWin's render loop froze (only
/// input events woke it — "tap to continue"), EGL surfaces corrupted
/// (eglSwapBuffers 0x3001), and KWin crash-looped (Graphics device lost).
#[derive(Default)]
pub struct WlClientState {
    pub compositor: CompositorClientState,
}

impl ClientData for WlClientState {}

/// SURFACE-FILTER: is `target` inside the surface tree rooted at `root`
/// (root itself, its subsurfaces, and recursively their children)? KWin
/// renders its wayland output through a subsurface CHILD of the xdg_toplevel,
/// not the toplevel surface itself — a plain `toplevel.wl_surface() == s`
/// check would filter out every real frame and starve KWin (it then fails
/// EGL init and reconnects in a loop). The cursor sprite is NOT in this
/// tree, so its frames stay filtered out.
fn surface_tree_contains(root: &WlSurface, target: &WlSurface) -> bool {
    if root == target {
        return true;
    }
    compositor::get_children(root)
        .iter()
        .any(|c| surface_tree_contains(c, target))
}

/// Extract the first new buffer from `surface`'s tree (own surface first,
/// then subsurfaces depth-first), releasing each consumed wl_buffer
/// immediately (ASYNC-RELEASE). Returns the extracted frame, if any.
fn extract_tree(surface: &WlSurface, frame_mem: &mut Option<FrameMem>) -> Option<ExtractedFrame> {
    let own = compositor::with_states(surface, |states| {
        let mut guard = states.cached_state.get::<SurfaceAttributes>();
        // Take the buffer out and clear the field — smithay's docs allow
        // this ("free to set this field to None to avoid processing it
        // several times").
        let buffer = guard.current().buffer.take();
        match buffer {
            Some(BufferAssignment::NewBuffer(wl_buffer)) => {
                let r = extract_frame(&wl_buffer, frame_mem);
                // ASYNC-RELEASE: the buffer is free now — the pixels were
                // dup'd/copied above. Dropping WlBuffer alone does NOT send
                // wl_buffer.release; smithay's own code always calls it
                // explicitly.
                wl_buffer.release();
                r
            }
            _ => None,
        }
    });
    if own.is_some() {
        return own;
    }
    for child in compositor::get_children(surface) {
        if let Some(f) = extract_tree(&child, frame_mem) {
            return Some(f);
        }
    }
    None
}

impl CompositorHandler for WlState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        // PER-CLIENT: one CompositorClientState per client, attached at
        // insert_client time. (The old thread_local single instance shared
        // state across all clients — see WlClientState docs.)
        &client
            .get_data::<WlClientState>()
            .expect("client inserted with WlClientState")
            .compositor
    }

    fn commit(&mut self, surface: &WlSurface) {
        let is_output_tree = self
            .toplevel
            .as_ref()
            .is_some_and(|t| surface_tree_contains(t.wl_surface(), surface));
        let top_id = self.toplevel.as_ref().map(|t| t.wl_surface().id());
        // DIAG: surface identity + tree match, so a single run shows whether
        // the committed surface is the toplevel, a tree child, or the
        // independent cursor sprite.
        tracing::info!(surface_id = ?surface.id(), is_output_tree, ?top_id, "surface commit");
        // SURFACE-FILTER: only the toplevel OUTPUT surface's frames go to
        // the App. KWin also commits auxiliary surfaces — most notably its
        // 32x32 cursor sprite surface (wl_pointer.set_cursor), which animates
        // at frame rate while the desktop is static. Forwarding those tiny
        // frames would let the App's latest-wins renderer overwrite the real
        // picture with a 32x32 cursor frame (black screen + dead panel).
        // Frame callbacks and presentation feedback still go out for every
        // surface, so KWin's per-surface render loops stay alive.
        // SURFACE-FILTER: only frames from the toplevel output surface TREE
        // go to the App. KWin renders its output through a subsurface child
        // of the xdg_toplevel (its own tree), and also owns an independent
        // 32x32 cursor sprite surface that animates at frame rate while the
        // desktop is static. Forwarding the cursor's tiny frames would let
        // the App's latest-wins renderer overwrite the real picture with a
        // 32x32 cursor frame (black screen + dead panel). Frame callbacks
        // and presentation feedback still go out for every surface, so
        // KWin's per-surface render loops stay alive.
        let is_output_tree = self
            .toplevel
            .as_ref()
            .is_some_and(|t| surface_tree_contains(t.wl_surface(), surface));
        // ASYNC-RELEASE: KWin's buffer is released IMMEDIATELY after
        // extraction. dmabuf frames are forwarded as a dup'd fd (the App's
        // fd is an independent reference — KWin reusing the buffer is safe);
        // SHM frames are copied into our own FrameMem. Holding KWin's buffer
        // until the App's RELEASE would exhaust KWin's EGL buffer pool and
        // crash it (eglSwapBuffers fails with EGL_BAD_SURFACE when no buffer
        // is available). The App's RELEASE is now purely a consumption
        // signal, not a KWin lifecycle gate.
        if is_output_tree {
            let extracted = extract_tree(surface, &mut self.frame_mem);

            match extracted {
                Some(ExtractedFrame::Dmabuf(bw, bh, fd)) => {
                    // FRAME-SIZE-FILTER: only frames matching the output
                    // size reach the App. KWin's cursor sprite is a
                    // subsurface INSIDE the toplevel tree (32x32, animates
                    // at frame rate while the desktop is static), so the
                    // surface-tree filter cannot exclude it — forwarding it
                    // would let the App's latest-wins renderer paint the
                    // tiny cursor frame as the whole picture.
                    if bw == self.screen_width && bh == self.screen_height {
                        tracing::info!(bw, bh, "dmabuf frame — fd forwarded directly (zero copy)");
                        if let Some(session) = &mut self.app_session {
                            let _ = session.send_frame(bw, bh, fd);
                        }
                    } else {
                        tracing::info!(bw, bh, "dropping non-output frame (cursor/aux)");
                    }
                }
                Some(ExtractedFrame::Shm(bw, bh, fd)) => {
                    if bw == self.screen_width && bh == self.screen_height {
                        tracing::info!(bw, bh, "SHM frame — copied into frame buffer");
                        if let Some(session) = &mut self.app_session {
                            let _ = session.send_frame(bw, bh, fd);
                        }
                    } else {
                        tracing::info!(bw, bh, "dropping non-output frame (cursor/aux)");
                    }
                }
                None => {
                    tracing::debug!("commit without a new buffer — nothing to send");
                }
            }
        } else {
            // SURFACE-FILTER: auxiliary surface (e.g. KWin's cursor sprite)
            // committed a frame — release its buffer and forward nothing.
            compositor::with_states(surface, |states| {
                let mut guard = states.cached_state.get::<SurfaceAttributes>();
                if let Some(BufferAssignment::NewBuffer(wl_buffer)) = guard.current().buffer.take() {
                    wl_buffer.release();
                }
            });
        }



        // VSYNC-PACING: collect this commit's frame callbacks and
        // presentation feedbacks into the pending queues — the vsync timer
        // (output refresh rate) flushes them at a steady beat. Dispatching
        // here, immediately, gives KWin an irregular event-driven signal
        // instead of a vsync.
        compositor::with_states(surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            let cbs: Vec<_> = guard.current().frame_callbacks.drain(..).collect();
            if !cbs.is_empty() {
                tracing::info!(count = cbs.len(), "vsync: queued frame callbacks");
                self.pending_callbacks.extend(cbs);
            }
        });
        compositor::with_states(surface, |states| {
            let mut guard = states.cached_state.get::<PresentationFeedbackCachedState>();
            let fbs: Vec<_> = guard.current().callbacks.drain(..).collect();
            if !fbs.is_empty() {
                self.pending_feedbacks.extend(fbs);
            }
        });
    }
}

impl WlState {
    /// VSYNC-PACING: called by the vsync timer at the output refresh rate
    /// (144Hz). Flushes all queued frame callbacks and presentation
    /// feedbacks with a single monotonic beat, giving KWin a stable vsync
    /// signal to drive its render loop (no more input-driven "tap to
    /// continue" fallback). No-op when nothing is queued.
    pub fn vsync_tick(&mut self) {
        if self.pending_callbacks.is_empty() && self.pending_feedbacks.is_empty() {
            return;
        }
        let elapsed = std::time::Instant::now().duration_since(self.clock_epoch);
        let now_ms = elapsed.as_millis() as u32;
        let period_ns = 1_000_000_000_000u64 / self.refresh_millihz.max(1) as u64;
        let refresh = Refresh::Fixed(std::time::Duration::from_nanos(period_ns));
        let seq = self.vsync_seq;
        self.vsync_seq += 1;

        let callbacks = std::mem::take(&mut self.pending_callbacks);
        let count = callbacks.len();
        for cb in callbacks {
            // Guard: the client may have disconnected while the callback was
            // queued — done() on a destroyed resource would misbehave.
            if cb.is_alive() {
                cb.done(now_ms);
            }
        }
        if count > 0 {
            tracing::info!(count, "vsync: dispatched frame callbacks");
        }
        for fb in self.pending_feedbacks.drain(..) {
            fb.presented(
                &self.output,
                elapsed,
                refresh,
                seq,
                wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync,
            );
        }
    }

    /// VSYNC-PACING: the refresh period for the vsync timer, derived from
    /// the App-reported refresh rate (CONF).
    pub fn vsync_period(&self) -> std::time::Duration {
        std::time::Duration::from_nanos(1_000_000_000_000u64 / self.refresh_millihz.max(1) as u64)
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
        self.output.enter(surface.wl_surface());
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
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        if let Some(pointer) = seat.get_pointer()
            && let Some(surface) = focused
        {
            let current = pointer.current_focus();
            if current.as_ref() != Some(surface) {
                let loc = pointer.current_location();
                let serial = {
                    let s = Serial::from(self.next_serial);
                    self.next_serial += 1;
                    s
                };
                pointer.motion(self, Some((surface.clone(), (0.0f64, 0.0f64).into())), &PointerMotionEvent {
                    location: loc,
                    serial,
                    time: 0,
                });
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::WlState;
    use smithay::backend::input::KeyState;

    #[test]
    fn key_down_maps_to_pressed() {
        assert_eq!(WlState::key_state_from_u32(1), KeyState::Pressed);
    }

    #[test]
    fn key_up_maps_to_released() {
        assert_eq!(WlState::key_state_from_u32(0), KeyState::Released);
    }

    #[test]
    fn malformed_state_defaults_to_released() {
        assert_eq!(WlState::key_state_from_u32(7), KeyState::Released);
        assert_eq!(WlState::key_state_from_u32(u32::MAX), KeyState::Released);
    }

    #[test]
    fn bucket_dpi_maps_panel_289_to_288() {
        assert_eq!(WlState::bucket_dpi(289), 288, "panel DPI must bucket to the 2x integer");
    }

    #[test]
    fn bucket_dpi_exact_values_pass_through() {
        for dpi in [96u32, 120, 144, 160, 192, 240, 288, 384] {
            assert_eq!(WlState::bucket_dpi(dpi), dpi, "exact bucket must be identity");
        }
    }

    #[test]
    fn bucket_dpi_rounds_to_nearest_bucket() {
        assert_eq!(WlState::bucket_dpi(90), 96, "below the smallest bucket rounds up");
        assert_eq!(WlState::bucket_dpi(110), 120, "midway rounds to the larger bucket");
        assert_eq!(WlState::bucket_dpi(0), 96, "degenerate input clamps to the smallest");
        assert_eq!(WlState::bucket_dpi(1000), 384, "above the largest bucket saturates");
    }
}

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
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
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
use smithay::wayland::presentation::{PresentationState, PresentationFeedbackCachedState};
use smithay::wayland::presentation::Refresh;
use tracing::info;
use wayland_protocols::xdg::shell::server::xdg_toplevel;

use crate::app_link::AppSession;
use crate::frame_cache::FrameCache;
use crate::touch::TouchInjector;
use wl_android_common::proto::{TouchMessage, TOUCH_PHASE_DOWN, TOUCH_PHASE_MOVE, TOUCH_PHASE_UP};

#[derive(Debug)]
enum ExtractedFrame {
    /// SHM frame extracted directly into the FrameCache memfd. The fd is
    /// None when the frame was DROPPED (target buffer still in the App's
    /// hands — latest-wins, no fd to send).
    Shm(u32, u32, Option<OwnedFd>),
}




/// Convert smithay's per-commit surface damage into pixel rects (clamped to
/// the frame), so the FrameCache can copy only the changed regions
/// (PERF-DAMAGE). `Buffer` damage is already in pixels; `Surface` damage is
/// logical and must be scaled by the surface's buffer_scale. Rectangles are
/// clamped into [0, w] × [0, h]; anything fully outside is dropped.
fn damage_to_rects(damage: &[compositor::Damage], scale: i32, w: u32, h: u32) -> Vec<crate::frame_cache::Rect> {
    let mut out = Vec::new();
    for d in damage {
        let (x, y, rw, rh): (i32, i32, i32, i32) = match d {
            compositor::Damage::Buffer(r) => (r.loc.x, r.loc.y, r.size.w, r.size.h),
            compositor::Damage::Surface(r) => (
                r.loc.x * scale,
                r.loc.y * scale,
                r.size.w * scale,
                r.size.h * scale,
            ),
        };
        let x = x.max(0);
        let y = y.max(0);
        let rw = rw.max(0);
        let rh = rh.max(0);
        if x >= w as i32 || y >= h as i32 || rw == 0 || rh == 0 {
            continue;
        }
        let rw = rw.min(w as i32 - x);
        let rh = rh.min(h as i32 - y);
        out.push(crate::frame_cache::Rect { x: x as u32, y: y as u32, w: rw as u32, h: rh as u32 });
    }
    out
}

/// Extract a committed buffer's pixels into the FrameCache memfd, returning
/// the pixel fd for the App. Two sources:
///
/// * SHM: copy straight out of the pool (PERF-12, damage-optimized).
/// * dmabuf (KWin GPU rendering): map plane 0 and copy — only LINEAR
///   buffers are CPU-readable; QCOM_COMPRESSED buffers are dropped (the App
///   is a CPU consumer and cannot import them).
fn extract_from_buffer(
    wl_buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    frame_cache: &mut Option<FrameCache>,
    damage: &[compositor::Damage],
    buffer_scale: i32,
) -> Option<ExtractedFrame> {
    // dmabuf path (KWin EGL): linear → mmap + copy.
    if let Ok(dmabuf) = smithay::wayland::dmabuf::get_dmabuf(wl_buffer) {
        return extract_from_dmabuf(dmabuf, frame_cache, damage, buffer_scale);
    }

    // SHM path (KWin software rendering).
    let shm_result = shm::with_buffer_contents(wl_buffer, |ptr, _pool_len, data| {
        let stride = data.stride as usize;
        let height = data.height as usize;
        if height == 0 || stride == 0 { return None; }
        if stride % 4 != 0 {
            tracing::warn!(stride, "non-multiple-of-4 SHM stride — dropping frame");
            return None;
        }
        let offset = data.offset as usize;
        let w = (stride as u32) / 4;
        if frame_cache.is_none() {
            match FrameCache::new(w, height as u32) {
                Ok(c) => *frame_cache = Some(c),
                Err(e) => {
                    tracing::error!(err = %e, "FrameCache::new failed");
                    return None;
                }
            }
        }
        let cache = frame_cache.as_mut().unwrap();
        cache.set_dimensions(w, height as u32);
        let rects = damage_to_rects(damage, buffer_scale, w, height as u32);
        // PERF-12 + PERF-DAMAGE: copy the damaged rects straight out of the
        // pool into the resident memfd mapping — no intermediate Vec
        // allocation + memcpy, and untouched regions are preserved from the
        // previous frame (the FrameCache accumulates damage per buffer).
        cache.push_damaged(w, height as u32, &rects, |dst, effective| {
            for r in effective {
                let row_bytes = r.w as usize * 4;
                for y in r.y..r.y + r.h {
                    let src = unsafe { ptr.add(offset + y as usize * stride + r.x as usize * 4) };
                    let dst_row = y as usize * stride;
                    // dst stride == src stride == w*4 (FrameCache built at
                    // w = stride/4), so the same row math applies.
                    if dst_row + row_bytes <= dst.len() && r.x as usize * 4 + row_bytes <= stride {
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                src,
                                dst.as_mut_ptr().add(dst_row + r.x as usize * 4),
                                row_bytes,
                            );
                        }
                    }
                }
            }
        })
    }).unwrap_or(None);
    let (sw, sh) = shm::with_buffer_contents(&wl_buffer, |_ptr, _len, data| {
        (data.stride as u32 / 4, data.height as u32)
    }).unwrap_or((0, 0));
    if shm_result.is_some() {
            return Some(ExtractedFrame::Shm(sw, sh, shm_result));
        }



    None
}

/// dmabuf source: map plane 0 (LINEAR only) and copy into the FrameCache.
/// Returns None when the buffer is compressed (not CPU-readable) or the map
/// fails — the frame is dropped, the next linear frame proceeds normally.
fn extract_from_dmabuf(
    dmabuf: &smithay::backend::allocator::dmabuf::Dmabuf,
    frame_cache: &mut Option<FrameCache>,
    damage: &[compositor::Damage],
    buffer_scale: i32,
) -> Option<ExtractedFrame> {
    use smithay::backend::allocator::Buffer;
    use smithay::backend::allocator::dmabuf::{DmabufMappingMode, DmabufSyncFlags};

    // Only LINEAR dmabufs are CPU-readable. Compressed (UBWC) buffers would
    // need GPU import — the App is a CPU consumer, so drop them.
    if dmabuf.has_modifier() {
        tracing::warn!(
            modifier = ?dmabuf.format().modifier,
            "dmabuf with non-linear modifier — CPU read-back unsupported, dropping frame"
        );
        return None;
    }

    let w = dmabuf.width();
    let h = dmabuf.height();
    if w == 0 || h == 0 {
        return None;
    }

    let mapping = match dmabuf.map_plane(0, DmabufMappingMode::READ) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(err = ?e, "dmabuf map_plane failed — dropping frame");
            return None;
        }
    };
    let stride = match dmabuf.strides().next() {
        Some(s) => s as usize,
        None => {
            tracing::warn!("dmabuf without plane 0 stride — dropping frame");
            return None;
        }
    };
    // dmabuf_sync (DMA_BUF_IOCTL_SYNC): START before the copy waits for the
    // GPU to finish writing (implicit fence); END after the copy tells the
    // kernel the CPU is done reading so the next GPU write is not blocked.
    // Both are required — skipping END would leave the read fence held.
    let sync_start = dmabuf.sync_plane(0, DmabufSyncFlags::READ | DmabufSyncFlags::START);
    if let Err(e) = &sync_start {
        tracing::warn!(err = ?e, "dmabuf sync START failed — reading may race the GPU");
    }

    let src_ptr = mapping.ptr() as *const u8;
    let cache = if frame_cache.is_none() {
        match FrameCache::new(w, h) {
            Ok(c) => {
                *frame_cache = Some(c);
                frame_cache.as_mut().unwrap()
            }
            Err(e) => {
                tracing::error!(err = %e, "FrameCache::new failed");
                return None;
            }
        }
    } else {
        frame_cache.as_mut().unwrap()
    };
    cache.set_dimensions(w, h);
    let rects = damage_to_rects(damage, buffer_scale, w, h);
    // EGL renders with GL origin at the bottom-left; the dmabuf is flagged
    // Y_INVERT so the compositor flips rows. Without the flip the picture
    // would be upside down. Damage rects use Wayland (top-left) coordinates
    // — the flip applies to the SOURCE row only.
    let y_inverted = dmabuf.y_inverted();
    let src_h = h as usize;
    let result = cache.push_damaged(w, h, &rects, |dst, effective| {
        // Bounds use the rectangle width for the copy but the FULL row width
        // for the stride-check — the source row may have padding after the
        // last pixel column, so requiring the whole row in-bounds is
        // conservative and safe.
        let row_bytes = w as usize * 4;
        for r in effective {
            for y in r.y..r.y + r.h {
                // Source row: flipped when the dmabuf is Y_INVERT.
                let src_y = if y_inverted { src_h - 1 - y as usize } else { y as usize };
                let src_row = src_y * stride;
                let dst_row = y as usize * row_bytes;
                if src_row + row_bytes <= mapping.length()
                    && dst_row + row_bytes <= dst.len()
                    && r.x as usize * 4 + r.w as usize * 4 <= stride
                {
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            src_ptr.add(src_row + r.x as usize * 4),
                            dst.as_mut_ptr().add(dst_row + r.x as usize * 4),
                            r.w as usize * 4,
                        );
                    }
                }
            }
        }
    });

    // END after the copy (even on copy failure — the mapping is still valid).
    if let Err(e) = dmabuf.sync_plane(0, DmabufSyncFlags::READ | DmabufSyncFlags::END) {
        tracing::warn!(err = ?e, "dmabuf sync END failed");
    }

    result.map(|fd| ExtractedFrame::Shm(w, h, Some(fd)))
}

pub struct WlState {
    pub display: Display<Self>,
    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
    pub xdg_shell_state: XdgShellState,
    pub dmabuf_state: smithay::wayland::dmabuf::DmabufState,
    pub frame_cache: Option<FrameCache>,
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
    /// Last Instant a frame was actually sent to the App — pacing anchor.
    pub last_send: std::time::Instant,
    pub output: Output,
    pub toplevel: Option<ToplevelSurface>,
    pub seat_state: SeatState<Self>,
    pub seat: Seat<Self>,
    pub touch_injector: TouchInjector,
    pub next_serial: u32,
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
            frame_cache: None,
            app_session: None, land_listener: None, land_source: None,
            clock_epoch: std::time::Instant::now(),
            screen_width: w, screen_height: h, refresh_millihz: refresh, dpi,
            frame_mode: 0, last_send: std::time::Instant::now(),
            output, toplevel: None, seat_state, seat, touch_injector,
            next_serial: 1,
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

    /// PACING: decide whether a freshly-committed frame may be sent to the App
    /// now, and (if sent) update the pacing anchor. Modes (ConfigMessage):
    /// 0 free — always send.
    /// 1 vsync-align — send at most one frame per refresh period; a commit
    ///   inside the same period is merged into the next tick (latest-wins).
    /// 2 performance — always send, minimum buffering (no pacing gate).
    /// 3 power-save — cap at half the refresh rate (fewer presents, less
    ///   GPU + copy work in the App; still smooth for static content).
    /// Returns true when the frame should be delivered.
    pub fn pacing_gate(&mut self) -> bool {
        let period = match self.frame_mode {
            0 | 2 => return true,
            1 => {
                let p = 1_000_000_000_000u64 / self.refresh_millihz.max(1) as u64;
                p.min(1_000_000_000) // cap at 1s: a 0/absent refresh never stalls
            }
            3 => {
                let p = 2_000_000_000_000u64 / self.refresh_millihz.max(1) as u64;
                p.min(2_000_000_000)
            }
            _ => return true,
        };
        let elapsed = self.last_send.elapsed().as_nanos() as u64;
        if elapsed >= period {
            self.last_send = std::time::Instant::now();
            true
        } else {
            false
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
        // PACING: when the gate says "don't send this frame", skip the whole
        // extract — writing the FrameCache would mark the buffer in flight
        // without ever sending its fd, wedging it forever (the App never
        // learns about a frame it never received). Skipped frames keep the
        // KWin buffer released (ASYNC-RELEASE below) and the next commit
        // proceeds normally.
        if !self.pacing_gate() {
            tracing::debug!("frame gated by pacing — skipped");
            return;
        }
        {
            let extracted: Option<ExtractedFrame> = 'extract: {
                let parent_data = compositor::with_states(surface, |states| {
                    let mut guard = states.cached_state.get::<SurfaceAttributes>();
                    // ASYNC-RELEASE: take the buffer out and clear the field —
                    // smithay's docs allow this ("free to set this field to
                    // None to avoid processing it several times"). The buffer
                    // is released once we're done with it, so KWin never
                    // blocks waiting for its buffer pool. If the buffer is a
                    // fresh one we haven't copied yet, extract it here.
                    let buffer = guard.current().buffer.take();
                    // PERF-DAMAGE: this commit's damage rects (smithay moves
                    // them into `current` on commit) drive the partial copy.
                    // `Damage` is not Clone; take() empties the vec, which is
                    // fine — commit handling already consumed it.
                    let damage = std::mem::take(&mut guard.current().damage);
                    let scale = guard.current().buffer_scale;
                    match buffer {
                        Some(BufferAssignment::NewBuffer(wl_buffer)) => {
                            let r = extract_from_buffer(&wl_buffer, &mut self.frame_cache, &damage, scale);
                            // ASYNC-RELEASE: the frame is now copied into the
                            // resident FrameCache (our shared memory), so KWin's
                            // buffer is free — release it immediately. Dropping
                            // WlBuffer alone does NOT send wl_buffer.release;
                            // smithay's own code always calls it explicitly.
                            wl_buffer.release();
                            r
                        }
                        _ => None,
                    }
                });
                if parent_data.is_some() { break 'extract parent_data; }
                let children = compositor::get_children(surface);
                for child in &children {
                    let child_data = compositor::with_states(child, |states| {
                        let mut guard = states.cached_state.get::<SurfaceAttributes>();
                        let buffer = guard.current().buffer.take();
                        let damage = std::mem::take(&mut guard.current().damage);
                        let scale = guard.current().buffer_scale;
                        match buffer {
                            Some(BufferAssignment::NewBuffer(wl_buffer)) => {
                                let r = extract_from_buffer(&wl_buffer, &mut self.frame_cache, &damage, scale);
                                wl_buffer.release();
                                r
                            }
                            _ => None,
                        }
                    });
                    if child_data.is_some() { break 'extract child_data; }
                }
                None
            };

            match extracted {
                Some(ExtractedFrame::Shm(bw, bh, fd)) => {
                    tracing::info!(bw, bh, "frame extracted from SHM");
                    // push_damaged returned None when the target buffer was
                    // still in the App's hands (frame dropped, latest-wins).
                    if let Some(fd) = fd {
                        if let Some(session) = &mut self.app_session {
                            tracing::info!(bw, bh, "sending frame to App");
                            let _ = session.send_frame(bw, bh, fd);
                        }
                    }
                }
                None => {
                    // No new buffer on this commit (KWin reused one or the
                    // commit carried no buffer): nothing to send — the App
                    // already holds the current frame. Just make sure the
                    // FrameCache exists for the next real frame.
                    tracing::debug!("commit without a new buffer — nothing to send");
                    if self.frame_cache.is_none() {
                        match FrameCache::new(self.screen_width, self.screen_height) {
                            Ok(c) => self.frame_cache = Some(c),
                            Err(e) => tracing::error!(err = %e, "FrameCache::new failed"),
                        }
                    }
                }
            }
        }



        let now_ms = std::time::Instant::now()
            .duration_since(self.clock_epoch)
            .as_millis() as u32;
        let period_ns = 1_000_000_000_000u64 / self.refresh_millihz as u64;
        let refresh = Refresh::Fixed(std::time::Duration::from_nanos(period_ns));
        let seq = self.frame_cache.as_ref().map(|c| c.seq()).unwrap_or(0);
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
    use super::{ExtractedFrame, FrameCache, extract_from_dmabuf, WlState};
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

    /// Build a LINEAR dmabuf backed by a memfd with a recognizable pattern:
    /// each row contains the row index byte. `flags` controls Y_INVERT so
    /// both orientations can be tested.
    fn build_pattern_dmabuf(flags: smithay::backend::allocator::dmabuf::DmabufFlags) -> smithay::backend::allocator::dmabuf::Dmabuf {
        use std::os::fd::FromRawFd;

        const W: u32 = 4;
        const H: u32 = 4;
        let size = (W * H * 4) as usize;
        let name = std::ffi::CString::new("test-dmabuf").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        assert_eq!(unsafe { libc::ftruncate(fd, size as libc::off_t) }, 0);
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        for y in 0..H {
            for x in 0..W * 4 {
                unsafe { *(ptr as *mut u8).add((y * W * 4 + x) as usize) = y as u8 };
            }
        }
        unsafe { libc::munmap(ptr, size) };

        let mut builder = smithay::backend::allocator::dmabuf::Dmabuf::builder(
            smithay::utils::Size::from((W as i32, H as i32)),
            drm_fourcc::DrmFourcc::Abgr8888,
            drm_fourcc::DrmModifier::Linear,
            flags,
        );
        assert!(builder.add_plane(owned, 0, 0, W * 4));
        builder.build().expect("dmabuf")
    }

    /// Read the FrameCache memfd produced for a dmabuf and return it as
    /// rows of bytes (row index, 4-byte pixels).
    fn extract_and_read_rows(
        dmabuf: &smithay::backend::allocator::dmabuf::Dmabuf,
        cache: &mut Option<FrameCache>,
    ) -> (u32, u32, Vec<Vec<u8>>) {
        use std::os::fd::AsRawFd;

        let frame = extract_from_dmabuf(dmabuf, cache, &[], 1);
        let (fw, fh, fd_opt) = match frame {
            Some(ExtractedFrame::Shm(w, h, fd)) => (w, h, fd),
            other => panic!("expected Shm frame, got {other:?}"),
        };
        let fd = fd_opt.expect("dmabuf frame must carry a pixel fd");
        let st = unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            assert_eq!(libc::fstat(fd.as_raw_fd(), &mut st), 0);
            st
        };
        let rp = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                st.st_size as usize,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        assert_ne!(rp, libc::MAP_FAILED);
        let bytes = unsafe { std::slice::from_raw_parts(rp as *const u8, st.st_size as usize) };
        let mut rows = Vec::new();
        for y in 0..fh {
            let start = (y * fw * 4) as usize;
            rows.push(bytes[start..start + (fw * 4) as usize].to_vec());
        }
        unsafe { libc::munmap(rp, st.st_size as usize) };
        (fw, fh, rows)
    }

    /// A LINEAR dmabuf WITHOUT Y_INVERT copies rows straight through.
    #[test]
    fn extract_from_linear_dmabuf_reads_pixels() {
        use smithay::backend::allocator::dmabuf::DmabufFlags;

        let dmabuf = build_pattern_dmabuf(DmabufFlags::empty());
        let mut cache: Option<FrameCache> = None;
        let (w, h, rows) = extract_and_read_rows(&dmabuf, &mut cache);
        assert_eq!((w, h), (4, 4));
        for y in 0..4u32 {
            assert_eq!(rows[y as usize][0], y as u8, "row {y} copied straight through");
            assert!(rows[y as usize].iter().all(|&b| b == y as u8), "row {y} all same byte");
        }
    }

    /// A LINEAR dmabuf WITH Y_INVERT (EGL default) must flip rows: source
    /// row 0 lands in target row 3, etc. Damage rects keep Wayland
    /// (top-left) coordinates — only the source row index flips.
    #[test]
    fn extract_from_yinverted_dmabuf_flips_rows() {
        use smithay::backend::allocator::dmabuf::DmabufFlags;

        let dmabuf = build_pattern_dmabuf(DmabufFlags::Y_INVERT);
        let mut cache: Option<FrameCache> = None;
        let (w, h, rows) = extract_and_read_rows(&dmabuf, &mut cache);
        assert_eq!((w, h), (4, 4));
        for y in 0..4u32 {
            // Source row (3 - y) contains byte (3 - y); after flip target row
            // y must contain byte (3 - y).
            assert_eq!(
                rows[y as usize][0],
                (3 - y) as u8,
                "row {y} must hold flipped source row {}",
                3 - y
            );
        }
    }
}

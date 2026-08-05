use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, OwnedFd};
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
use smithay::backend::allocator::Buffer;
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
use smithay::wayland::dmabuf::{get_dmabuf, DmabufState};
use smithay::wayland::content_type::ContentTypeState;
use smithay::wayland::alpha_modifier::AlphaModifierState;
use smithay::wayland::pointer_constraints::{PointerConstraintsHandler, PointerConstraintsState};
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::fractional_scale::FractionalScaleHandler;
use smithay::wayland::presentation::{PresentationState, PresentationFeedbackCachedState};
use smithay::wayland::presentation::Refresh;
use tracing::info;
use wayland_protocols::xdg::shell::server::xdg_toplevel;

use crate::app_link::{AppSession, SessionMode};
use crate::blit::BlitEngine;
use crate::frame_cache::FrameCache;
use crate::frame_router::FrameRouter;
use crate::touch::TouchInjector;
use wl_android_common::proto::{TouchMessage, TOUCH_PHASE_DOWN, TOUCH_PHASE_MOVE, TOUCH_PHASE_UP};

enum ExtractedFrame {
    /// SHM frame extracted directly into the FrameCache memfd (PERF-12:
    /// single copy out of the pool — no intermediate Vec). Carries the
    /// dup'd memfd already filled with this frame's pixels.
    Shm(u32, u32, OwnedFd),
    Dmabuf {
        w: u32,
        h: u32,
        fd: OwnedFd,
        /// Per-wl_buffer key (unlike the surface-hash `buffer_id` sent to the
        /// App): the PERF-11 import cache must distinguish the swapchain's
        /// wl_buffers, which all share one surface hash.
        buf_key: u32,
        vk_format: ash::vk::Format,
        modifier: u64,
    },
}

fn fourcc_to_vk(fourcc: drm_fourcc::DrmFourcc) -> ash::vk::Format {
    match fourcc {
        drm_fourcc::DrmFourcc::Argb8888 | drm_fourcc::DrmFourcc::Xrgb8888 => {
            ash::vk::Format::B8G8R8A8_UNORM
        }
        drm_fourcc::DrmFourcc::Abgr8888 | drm_fourcc::DrmFourcc::Xbgr8888 => {
            ash::vk::Format::R8G8B8A8_UNORM
        }
        _ => ash::vk::Format::B8G8R8A8_UNORM,
    }
}

fn extract_from_buffer(
    wl_buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    frame_cache: &mut Option<FrameCache>,
) -> Option<ExtractedFrame> {
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
        // PERF-12: copy straight out of the pool into the resident memfd
        // mapping — no intermediate Vec allocation + memcpy.
        cache.push_from(w, height as u32, |dst| {
            for y in 0..height {
                let src = unsafe { ptr.add(offset + y * stride) };
                let dst_row = y * stride;
                if dst_row + stride <= dst.len() {
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            src,
                            dst.as_mut_ptr().add(dst_row),
                            stride,
                        );
                    }
                }
            }
        })
    }).unwrap_or(None);
    let (sw, sh) = shm::with_buffer_contents(&wl_buffer, |_ptr, _len, data| {
        (data.stride as u32 / 4, data.height as u32)
    }).unwrap_or((0, 0));
    if let Some(fd) = shm_result {
            return Some(ExtractedFrame::Shm(sw, sh, fd));
        }

    if let Ok(dmabuf) = get_dmabuf(&wl_buffer) {
        let w = dmabuf.width();
        let h = dmabuf.height();
        let format = dmabuf.format();
        let modifier: u64 = u64::from(format.modifier);
        let vk_format = fourcc_to_vk(format.code);
        let buf_key = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            wl_buffer.hash(&mut hasher);
            hasher.finish() as u32
        };
        let handles: Vec<_> = dmabuf.handles().collect();
        if let Some(handle) = handles.first() {
            let raw_fd = handle.as_raw_fd();
            let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw_fd) };
            if let Ok(fd) = borrowed.try_clone_to_owned() {
                return Some(ExtractedFrame::Dmabuf { w, h, fd, buf_key, vk_format, modifier });
            }
        }
    }

    None
}

/// TODO 23: dmabuf frames stashed at commit time, keyed by per-wl_buffer id,
/// waiting for the router's EnqueueFrame to drive the blit. Replaces the
/// unkeyed `pending_pixel_fds` (which could pair an fd with the wrong frame).
#[derive(Default)]
pub struct PendingFrames {
    map: HashMap<u32, (OwnedFd, u32, u32, ash::vk::Format, u64)>,
}

impl PendingFrames {
    pub fn stash(
        &mut self, key: u32, fd: OwnedFd, w: u32, h: u32,
        format: ash::vk::Format, modifier: u64,
    ) {
        self.map.insert(key, (fd, w, h, format, modifier));
    }

    pub fn take(&mut self, key: u32) -> Option<(OwnedFd, u32, u32, ash::vk::Format, u64)> {
        self.map.remove(&key)
    }

    /// C-02/AppLost: dropping the map closes every stashed dmabuf fd.
    pub fn clear(&mut self) {
        self.map.clear();
    }

    #[allow(dead_code)] // test-only for now; TODO 24 will need them
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[allow(dead_code)] // test-only for now; TODO 24 will need them
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// F-11 cum-ack: every slot whose frame serial ≤ `ack` returns to the free
/// pool. Returns the freed (slot, fence) pairs so the caller can destroy the
/// fences (signaled by then — the App waited on the exported sync_file).
pub fn free_slots_on_ack<F: Copy>(in_use: &mut Vec<(u32, u64, F)>, ack: u64) -> Vec<(u32, F)> {
    let mut freed = Vec::new();
    in_use.retain(|&(slot, serial, fence)| {
        if serial <= ack {
            freed.push((slot, fence));
            false
        } else {
            true
        }
    });
    freed
}

/// F-14: per-slot BRDY readiness set. A slot becomes blittable only when it is
/// in this set. Two producers: (1) TBUF registration (main.rs SlotRegistration
/// branch) — the App cannot BRDY a slot before it has presented a frame from
/// it, so the FIRST frame after registration is implicitly granted (deadlock
/// resolution); and (2) explicit BRDY (`handle_brdy`) after the App presents a
/// fence frame and releases the slot for reuse.
///
/// `blit_and_send_frame` consumes the flag on a successful blit; every reuse
/// requires a fresh BRDY (F-14). `free_slots_on_ack` does NOT re-add — an ack
/// returns the slot to the free pool, but blitting into it still waits for the
/// App's BRDY.
#[derive(Debug, Default)]
pub struct SlotReadySet {
    ready: HashSet<u32>,
}

impl SlotReadySet {
    pub fn mark_ready(&mut self, slot: u32) {
        self.ready.insert(slot);
    }

    /// Consume the ready flag for `slot`. Returns true iff the slot was ready
    /// (and is now consumed); a double-consume returns false.
    pub fn consume(&mut self, slot: u32) -> bool {
        self.ready.remove(&slot)
    }

    pub fn clear(&mut self) {
        self.ready.clear();
    }

    pub fn is_ready(&self, slot: u32) -> bool {
        self.ready.contains(&slot)
    }
}

/// F-14: a slot is blittable iff it is registered in the blit engine, the App
/// declared it ready (BRDY or initial registration), and no unacked frame is
/// in flight on it. The `in_use` check is implied by the ready-set consumption
/// (a consumed slot is not ready until the next BRDY) but kept as a defensive
/// double-check against an early BRDY-before-ack.
pub fn slot_blittable(
    slot: u32,
    slots: &HashMap<u32, u64>,
    ready: &SlotReadySet,
    in_use: &HashSet<u32>,
) -> bool {
    slots.contains_key(&slot) && ready.is_ready(slot) && !in_use.contains(&slot)
}

/// LAND_MODE=shm forces the legacy SHM/CPU frame path (frame_cache memfd
/// triple-buffer + pixel-fd frames). Any other value (unset / auto / blit)
/// retires it: SHM frames are logged and dropped, blit is the only frame
/// producer. Debug fallback only — KWin must be configured to produce dmabufs
/// (the doctor/deploy scripts set the right env).
pub fn shm_path_enabled() -> bool {
    std::env::var("LAND_MODE").map(|v| v == "shm").unwrap_or(false)
}

pub struct WlState {
    pub display: Display<Self>,
    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
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
    pub frame_cache: Option<FrameCache>,
    pub dmabuf_state: DmabufState,
    pub blit_engine: BlitEngine,
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
    pub output: Output,
    pub toplevel: Option<ToplevelSurface>,
    pub seat_state: SeatState<Self>,
    pub seat: Seat<Self>,
    pub touch_injector: TouchInjector,
    pub next_serial: u32,
    pub pending_frames: PendingFrames,
    /// PERF-11: per-wl_buffer import cache (buf_key → vk image handle).
    pub frame_images: HashMap<u32, u64>,
    /// (slot, frame serial, submit fence) — in use from blit-submit until the
    /// serial is cum-acked (F-11), then the slot frees and the fence dies.
    pub slots_in_use: Vec<(u32, u64, ash::vk::Fence)>,
    /// F-14: slots the App declared writable (BRDY) or that are ready by
    /// initial TBUF registration. blit_and_send_frame consumes a slot's flag
    /// on a successful blit; reuse requires the App's next BRDY.
    pub brdy_ready: SlotReadySet,
}

/// PERF-11 import-cache bound: a destroyed wl_buffer's entry lingers (the
/// comp/dmabuf buffer_destroyed lane cannot reach this map — documented gap),
/// so an arbitrary entry is evicted past the bound instead of leaking GPU memory.
const MAX_FRAME_IMAGES: usize = 8;

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
            display, compositor_state, shm_state, single_pixel_buffer_state,
            viewporter_state, content_type_state, alpha_modifier_state, pointer_constraints_state,
            fractional_scale_state, presentation_state,
            xdg_shell_state, output_state, frame_router, frame_cache: None, blit_engine,
            dmabuf_state,
            app_session: None, land_listener: None, land_source: None,
            clock_epoch: std::time::Instant::now(),
            screen_width: w, screen_height: h, refresh_millihz: refresh, dpi,
            output, toplevel: None, seat_state, seat, touch_injector,
            next_serial: 1,
            pending_frames: PendingFrames::default(),
            frame_images: HashMap::new(),
            slots_in_use: Vec::new(),
            brdy_ready: SlotReadySet::default(),
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

    /// F-14: the App presented the previous fence frame from `slot` and
    /// releases it for reuse — the slot becomes eligible for the next blit.
    /// Registration itself also marks the slot ready (main.rs
    /// SlotRegistration branch): the App cannot BRDY a slot before it owns a
    /// frame, so the FIRST frame is implicitly granted. Idempotent.
    pub fn handle_brdy(&mut self, slot: u32) {
        self.brdy_ready.mark_ready(slot);
        tracing::info!(slot, "slot declared ready (BRDY)");
    }

    /// TODO 23: blit pipeline for a router-enqueued dmabuf frame. Takes the
    /// stashed dmabuf for `buffer_id`, imports it (cached per wl_buffer,
    /// PERF-11), blits into a free App slot against a fresh exportable fence,
    /// exports that fence as a sync_file, and sends a fence-only frame (F-08).
    /// No free slot / any blit failure → frame dropped; F-05 latest-wins
    /// retries on the next commit.
    pub fn blit_and_send_frame(&mut self, buffer_id: u32, serial: u64) {
        // Readiness before take: an early return must not consume the fd, or
        // the router's in_flight counts a frame that was never sent/acked.
        let active = match &self.app_session {
            Some(s) => s.mode() == SessionMode::Active,
            None => false,
        };
        if !active {
            return;
        }
        let Some((fd, w, h, format, modifier)) = self.pending_frames.take(buffer_id) else {
            return; // SHM-path commit or already processed — nothing stashed
        };

        let src = if let Some(&handle) = self.frame_images.get(&buffer_id) {
            handle
        } else {
            if self.frame_images.len() >= MAX_FRAME_IMAGES
                && let Some((&old_key, &old_handle)) = self.frame_images.iter().next()
            {
                self.frame_images.remove(&old_key);
                self.blit_engine.destroy_image(old_handle);
            }
            match self.blit_engine.import_dmabuf(fd, w, h, format, modifier) {
                Ok(handle) => {
                    self.frame_images.insert(buffer_id, handle);
                    handle
                }
                Err(e) => {
                    tracing::warn!(err = %e, buffer_id, "dmabuf import failed, frame dropped");
                    return;
                }
            }
        };
        // Cache hit: the stashed fd closes unused here; the imported VkImage
        // pins the dma-buf (the Vulkan import owns a dup of the fd).

        // F-14 BRDY gating: a slot is blittable iff registered, App-declared
        // ready (initial registration or explicit BRDY), and not in use.
        let in_use: HashSet<u32> = self.slots_in_use.iter().map(|(s, _, _)| *s).collect();
        let slot = self
            .blit_engine
            .slots
            .keys()
            .copied()
            .find(|slot| slot_blittable(*slot, &self.blit_engine.slots, &self.brdy_ready, &in_use));
        let Some(slot) = slot else {
            tracing::warn!(serial, "no BRDY-ready blit slot — frame dropped (latest-wins retries)");
            return;
        };
        let dst = self.blit_engine.slots[&slot];

        let fence = match self.blit_engine.create_exportable_fence() {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(err = %e, "create exportable fence failed");
                return;
            }
        };
        if let Err(e) = self.blit_engine.blit_submit_with_fence(src, dst, w, h, fence) {
            tracing::warn!(err = %e, "blit submit failed");
            self.blit_engine.destroy_fence_handle(fence);
            return;
        }
        let fence_fd = match self.blit_engine.export_fence_syncfd(fence) {
            Ok(fd) => fd, // Ok(None) = already signaled (F-13) → carries_fence=false
            Err(e) => {
                tracing::warn!(err = %e, "fence export failed");
                self.blit_engine.destroy_fence_handle(fence);
                return;
            }
        };
        self.slots_in_use.push((slot, serial, fence));
        // Consume the ready flag only on success — a failed blit leaves the
        // slot ready for the next frame (the App never received a frame to
        // present and therefore will not BRDY it).
        self.brdy_ready.consume(slot);
        tracing::info!(serial, slot, buffer_id, "blit frame → App (fence-only)");
        if let Some(session) = &mut self.app_session {
            let _ = session.send_frame(serial, slot, w, h, w, h, None, fence_fd);
        }
    }

    /// C-02: on App disconnect, drop all blit-pipeline state — stashed dmabuf
    /// fds close, cached per-buffer VkImages are destroyed, and in-flight
    /// submit fences are destroyed. (Slot images are owned by
    /// blit_engine.clear_slots, called separately.)
    pub fn clear_blit_pipeline_state(&mut self) {
        self.pending_frames.clear();
        self.brdy_ready.clear();
        let handles: Vec<u64> = self.frame_images.drain().map(|(_, h)| h).collect();
        for handle in handles {
            self.blit_engine.destroy_image(handle);
        }
        let fences: Vec<ash::vk::Fence> =
            self.slots_in_use.drain(..).map(|(_, _, fence)| fence).collect();
        for fence in fences {
            self.blit_engine.destroy_fence_handle(fence);
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
        let mut fed_router = false;
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
                    match buffer {
                        Some(BufferAssignment::NewBuffer(wl_buffer)) => {
                            let r = extract_from_buffer(&wl_buffer, &mut self.frame_cache);
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
                        match buffer {
                            Some(BufferAssignment::NewBuffer(wl_buffer)) => {
                                let r = extract_from_buffer(&wl_buffer, &mut self.frame_cache);
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
                    if !shm_path_enabled() {
                        // TODO 31: SHM/CPU path retired — blit is the only frame
                        // producer. KWin producing SHM here means dmabuf fell back;
                        // dropping is correct per the design (doctor/deploy set the
                        // env so KWin emits dmabufs). LAND_MODE=shm restores this.
                        tracing::warn!(
                            bw, bh,
                            "SHM frame dropped: LAND_MODE!=shm retires the CPU frame path \
                             (blit requires dmabuf; set LAND_MODE=shm for the debug fallback)",
                        );
                        // fed_router stays false → the bookkeeping Commit feed below
                        // keeps frame callbacks ticking (headless drain equivalent).
                    } else {
                    tracing::info!(bw, bh, "frame extracted from SHM");
                    let seq = self.frame_cache.as_ref().map(|c| c.seq()).unwrap_or(0);
                    // H-04: blit sends no frames until its slots are registered.
                    if let Some(session) = &mut self.app_session
                        && session.mode() == SessionMode::Active
                    {
                        let serial = seq;
                        tracing::info!(serial, bw, bh, "sending frame to App");
                        let _ = session.send_frame(
                            serial, buffer_id, bw, bh, bw, bh, Some(fd), None,
                        );
                    }
                    }
                }
                Some(ExtractedFrame::Dmabuf { w: bw, h: bh, fd, buf_key, vk_format, modifier }) => {
                    tracing::info!(bw, bh, buf_key, "frame extracted from DMABUF — stashed for blit");
                    // TODO 23: stash keyed by wl_buffer; the router decides
                    // (F-04 backpressure / F-05 latest-wins) and its EnqueueFrame
                    // action drives the blit pipeline in dispatch_router_actions.
                    self.pending_frames.stash(buf_key, fd, bw, bh, vk_format, modifier);
                    // H-04: the router counts every fed Commit as in_flight. Feeding
                    // it while the session is not yet Active (Handshake/
                    // SlotRegistration) would book serials that can never be sent
                    // and never acked → a permanent stall after activation. Headless
                    // (no session) still feeds so KWin keeps ticking via FireCallback.
                    let feedable = match &self.app_session {
                        None => true,
                        Some(s) => s.mode() == SessionMode::Active,
                    };
                    fed_router = true; // the DMABUF branch owns the router feed
                    if feedable {
                        let actions = self.frame_router.handle(
                            crate::frame_router::RouterEvent::Commit {
                                buffer_id: buf_key,
                                has_fds: true,
                                // The router assigns its own incremented serial on
                                // Commit; the anticipated one is passed through here.
                                serial: self.frame_router.current_serial() + 1,
                            },
                        );
                        crate::dispatch_router_actions(self, &actions);
                    }
                }
                None => {
                    tracing::warn!("no SHM or DMABUF buffer extracted on commit");
                    // 仍发一个空帧（带 fd），保持 SCM_RIGHTS 消息边界对齐
                    if self.frame_cache.is_none() {
                        match FrameCache::new(self.screen_width, self.screen_height) {
                            Ok(c) => self.frame_cache = Some(c),
                            Err(e) => tracing::error!(err = %e, "FrameCache::new failed"),
                        }
                    }
                    if let Some(cache) = &mut self.frame_cache {
                        if let Some(fd) = cache.current_frame() {
                            // H-04: blit sends no frames until its slots are registered.
                            if let Some(session) = &mut self.app_session
                                && session.mode() == SessionMode::Active
                            {
                                let (fd, seq, cw, ch) = fd;
                                let _ = session.send_frame(
                                    seq, buffer_id, self.screen_width, self.screen_height,
                                    cw, ch, Some(fd), None,
                                );
                            }
                        }
                    }
                }
            }
        }

        if !fed_router {
            // SHM / no-buffer paths keep the decorative bookkeeping feed (the
            // send already happened above); the blit dispatch finds no stash.
            let _actions = self.frame_router.handle(
                crate::frame_router::RouterEvent::Commit {
                    buffer_id,
                    has_fds: false,
                    serial: 0,
                },
            );
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
    use super::{free_slots_on_ack, PendingFrames, WlState, SlotReadySet, shm_path_enabled, slot_blittable};
    use smithay::backend::input::KeyState;
    use std::collections::{HashMap, HashSet};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    fn fake_fd() -> OwnedFd {
        UnixStream::pair().expect("socketpair").0.into()
    }

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

    // F-11: cumulative ack frees every slot whose frame serial ≤ ack and
    // returns their fences for destruction; newer frames stay in use.
    #[test]
    fn free_slots_on_ack_frees_cumulatively() {
        let mut in_use: Vec<(u32, u64, u32)> = vec![(1, 10, 100), (2, 11, 200), (0, 12, 300)];
        let mut freed = free_slots_on_ack(&mut in_use, 11);
        freed.sort();
        assert_eq!(freed, vec![(1u32, 100u32), (2, 200)]);
        assert_eq!(in_use, vec![(0u32, 12u64, 300u32)]);
    }

    #[test]
    fn free_slots_on_ack_with_zero_frees_nothing() {
        let mut in_use: Vec<(u32, u64, u32)> = vec![(1, 10, 100), (2, 11, 200)];
        let freed = free_slots_on_ack(&mut in_use, 0);
        assert!(freed.is_empty());
        assert_eq!(in_use.len(), 2);
    }

    // TODO 23: pending_frames is keyed by buffer (insert → take-by-key →
    // key gone; clear-on-lost empties the map and closes stashed fds).
    #[test]
    fn pending_frames_stash_take_clear() {
        let mut pf = PendingFrames::default();
        assert!(pf.is_empty());

        pf.stash(7, fake_fd(), 640, 480, ash::vk::Format::R8G8B8A8_UNORM, 0);
        assert_eq!(pf.len(), 1);

        let taken = pf.take(7).expect("stashed frame must be present");
        assert_eq!((taken.1, taken.2), (640, 480));
        assert!(pf.take(7).is_none(), "take removes the entry");
        assert!(pf.is_empty());

        pf.stash(1, fake_fd(), 100, 100, ash::vk::Format::R8G8B8A8_UNORM, 0);
        pf.stash(2, fake_fd(), 200, 200, ash::vk::Format::B8G8R8A8_UNORM, 5);
        assert_eq!(pf.len(), 2);
        pf.clear();
        assert!(pf.is_empty());
        assert_eq!(pf.len(), 0);
        assert!(pf.take(1).is_none());
    }

    // Same-key re-stash (latest-wins at the stash level) replaces the entry.
    #[test]
    fn pending_frames_restash_overwrites() {
        let mut pf = PendingFrames::default();
        pf.stash(9, fake_fd(), 111, 222, ash::vk::Format::R8G8B8A8_UNORM, 0);
        pf.stash(9, fake_fd(), 333, 444, ash::vk::Format::R8G8B8A8_UNORM, 0);
        assert_eq!(pf.len(), 1);
        let taken = pf.take(9).expect("entry present");
        assert_eq!((taken.1, taken.2), (333, 444), "newest stash wins");
    }

    // F-14: mark_ready then consume removes the slot; is_ready flips false.
    #[test]
    fn slot_ready_consume_removes() {
        let mut set = SlotReadySet::default();
        set.mark_ready(1);
        assert!(set.is_ready(1), "marked slot must be ready");
        assert!(set.consume(1), "consume returns true for a ready slot");
        assert!(!set.is_ready(1), "consume removes the slot from the set");
    }

    // F-14: consuming a never-ready slot is false, and a double-consume (after
    // the first removal) is also false — the flag is a one-shot grant per BRDY.
    #[test]
    fn slot_ready_double_consume_is_false() {
        let mut set = SlotReadySet::default();
        assert!(!set.consume(3), "consume on a never-ready slot is false");
        set.mark_ready(3);
        assert!(set.consume(3));
        assert!(!set.consume(3), "double-consume is false (one-shot grant)");
    }

    // F-14 / C-02: AppLost clears the whole set — no slot survives a session
    // teardown as ready.
    #[test]
    fn slot_ready_clear_empties() {
        let mut set = SlotReadySet::default();
        set.mark_ready(0);
        set.mark_ready(2);
        set.clear();
        assert!(!set.is_ready(0));
        assert!(!set.is_ready(2));
        assert!(!set.consume(0), "clear empties the ready set");
    }

    // F-14: a slot is blittable only when all three hold — registered, ready,
    // and not in use. Missing registration / missing BRDY / in-flight frame
    // each veto the blit.
    #[test]
    fn slot_blittable_requires_registered_ready_and_free() {
        let mut slots = HashMap::new();
        slots.insert(0u32, 100u64);
        slots.insert(1u32, 200u64);
        let mut ready = SlotReadySet::default();
        ready.mark_ready(0);
        let empty: HashSet<u32> = HashSet::new();
        assert!(slot_blittable(0, &slots, &ready, &empty), "registered+ready+free");
        assert!(!slot_blittable(1, &slots, &ready, &empty), "registered but not ready");
        assert!(!slot_blittable(2, &slots, &ready, &empty), "not registered at all");
        let in_use: HashSet<u32> = [0].into_iter().collect();
        assert!(!slot_blittable(0, &slots, &ready, &in_use), "ready but in use (early BRDY)");
    }

    // F-14: the ready flag is consumed by the blit — a slot is NOT blittable
    // again until the App's next BRDY re-arms it.
    #[test]
    fn slot_blittable_after_consume_is_false() {
        let mut slots = HashMap::new();
        slots.insert(5u32, 1u64);
        let mut ready = SlotReadySet::default();
        ready.mark_ready(5);
        assert!(ready.consume(5), "blit consumed the slot's ready flag");
        assert!(!slot_blittable(5, &slots, &ready, &HashSet::new()), "consumed ⇒ not blittable");
    }

    // TODO 31: LAND_MODE gate — only the exact "shm" value keeps the SHM/CPU
    // path alive. env is process-global: deterministic only under
    // --test-threads=1 (crate convention).
    #[test]
    fn shm_path_enabled_requires_exact_env() {
        unsafe { std::env::set_var("LAND_MODE", "shm") };
        assert!(shm_path_enabled(), "\"shm\" must enable the SHM path");
        unsafe { std::env::set_var("LAND_MODE", "auto") };
        assert!(!shm_path_enabled(), "\"auto\" must retire the SHM path");
        unsafe { std::env::set_var("LAND_MODE", "blit") };
        assert!(!shm_path_enabled(), "\"blit\" must retire the SHM path");
        unsafe { std::env::remove_var("LAND_MODE") };
        assert!(!shm_path_enabled(), "unset must retire the SHM path");
    }
}

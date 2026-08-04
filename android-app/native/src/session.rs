use std::ffi::CString;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use wl_android_common::proto;
use wl_android_common::proto::Message;

use crate::ahb::AhbSlot;

/// Read-ahead buffered reader for the App side of the land socket.
///
/// The transport is SOCK_STREAM (DESIGN.md P-01 deviation): the kernel may
/// split one length-prefixed message across recvmsg calls, or coalesce
/// several (a frame's FACK + a Touch/Key from the JNI path) into one. The
/// server's Transport buffers with `pending`; this mirrors that so the App
/// recv thread never sees a partial length prefix ("recv too short" crash —
/// previously triggered once input sends gained their own write path and
/// could interleave with FACK bytes).
struct PendingReader {
    stream: UnixStream,
    buf: Vec<u8>,
    pending: Vec<u8>,
    pending_fds: Vec<OwnedFd>,
}

impl PendingReader {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            buf: vec![0u8; 65536],
            pending: Vec::new(),
            pending_fds: Vec::new(),
        }
    }

    /// Read one complete length-prefixed message. Blocks until a full message
    /// is available (the caller drives the recv thread's loop). Returns the
    /// message body (length prefix stripped) plus any SCM_RIGHTS fds that
    /// belong to it — fds are delivered in byte order on SOCK_STREAM, and a
    /// message's fds arrive with or before its final bytes, so the first
    /// message in `pending` owns the leading fds.
    fn next_message(&mut self) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
        loop {
            if let Some((body, fds)) = Self::take_message(&mut self.pending, &mut self.pending_fds) {
                return Ok((body, fds));
            }
            // Not enough bytes for a full message — pull more from the socket.
            let (n_bytes, fds) = {
                let mut cmsg_space = nix::cmsg_space!([std::os::fd::RawFd; 4]);
                let mut iov = [std::io::IoSliceMut::new(&mut self.buf)];
                let msg = nix::sys::socket::recvmsg::<()>(
                    self.stream.as_raw_fd(),
                    &mut iov,
                    Some(&mut cmsg_space),
                    nix::sys::socket::MsgFlags::empty(),
                )?;
                let fds: Vec<OwnedFd> = msg
                    .cmsgs()
                    .map(|iter| {
                        iter.flat_map(|c| match c {
                            nix::sys::socket::ControlMessageOwned::ScmRights(fds) => fds,
                            _ => vec![],
                        })
                        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
                        .collect()
                    })
                    .unwrap_or_default();
                (msg.bytes, fds)
            };
            if n_bytes == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "socket closed"));
            }
            self.pending.extend_from_slice(&self.buf[..n_bytes]);
            self.pending_fds.extend(fds);
            // Loop: the recvmsg may have coalesced several messages, and the
            // first complete one is served now.
        }
    }

    /// Pop one complete length-prefixed message from the pending buffer.
    /// Returns None when the buffer holds fewer than 4 bytes or a partial body.
    fn take_message(pending: &mut Vec<u8>, pending_fds: &mut Vec<OwnedFd>) -> Option<(Vec<u8>, Vec<OwnedFd>)> {
        if pending.len() < 4 {
            return None;
        }
        let msg_len = u32::from_le_bytes([pending[0], pending[1], pending[2], pending[3]]) as usize;
        if pending.len() < 4 + msg_len {
            return None;
        }
        let body = pending[4..4 + msg_len].to_vec();
        let fds = std::mem::take(pending_fds);
        pending.drain(..4 + msg_len);
        Some((body, fds))
    }
}

#[allow(unused)]
fn dlog(tag: &str, msg: &str) {
    #[cfg(target_os = "android")]
    {
        let tag_c = CString::new(tag).unwrap_or_default();
        let msg_c = CString::new(msg).unwrap_or_default();
        unsafe {
            ndk_sys::__android_log_write(4, tag_c.as_ptr(), msg_c.as_ptr());
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (tag, msg);
        eprintln!("[{}] {}", tag, msg);
    }
}

fn safe_mmap_len(fd: &impl AsRawFd, requested: usize) -> usize {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
    if rc != 0 {
        log::error!(
            "fstat(frame fd) failed: {}; treating frame as empty",
            io::Error::last_os_error()
        );
        return 0;
    }
    (st.st_size as usize).min(requested)
}

/// Split a received Frame's fds per the wire contract (P-08/P-08b) and route
/// the frame to `on_frame`. Two dispatch paths:
///
/// * FENCE path (`carries_fence()` — the server blitted the frame straight
///   into a swapchain slot and only ships the sync_file fence): the LAST fd
///   is the fence (app_link.rs `send_frame` pushes pixel fds first, then the
///   fence; `proto::fd_count` counts planes then +1 fence). There is no CPU
///   mmap — the pixel payload already lives in the slot. `on_frame` receives
///   `fence_fd = Some(fence_fd)` (ownership transfers — the callback must
///   import it into Vulkan or drop it, F-12) and `pixels = &[]`. Any plane
///   fds carried alongside the fence (P-08b coexistable) are dropped: the App
///   consumes no pixel bytes for fence frames.
///
/// * LEGACY path (no fence — SHM pixel frames, pre-blit server): `fds[0]` is
///   the pixel dmabuf. It is fstat-guarded and mmap'd; `on_frame` receives
///   `fence_fd = None` and the mapped pixels. The slice aliases the mapping
///   and is only valid for the call (see the CONTRACT note in `run_loop`).
///
/// Verbatim copy host-tested in .omo/start-work/frame-loop-harness.
///
/// RENDER-DECOUPLE: the SHM pixel fd is handed to `on_frame` by ownership
/// (no mmap here) — the recv thread enqueues it and the render thread does
/// the mmap+copy. The fd is fstat-guarded for the expected size so a
/// truncated frame is reported instead of SIGBUSing in the render thread.
fn dispatch_frame(
    fm: &proto::FrameMessage,
    fds: Vec<OwnedFd>,
    on_frame: &impl Fn(u64, u32, u32, u32, Option<OwnedFd>, Option<OwnedFd>),
) {
    let size = fm.width as usize * fm.height as usize * 4;
    let mut fds = fds;
    if fm.carries_fence() {
        // Fence is the last fd (P-08b). decode() enforced fd_count, so pop()
        // yields the fence in practice; plane fds (if any) are dropped below.
        if let Some(fence) = fds.pop() {
            drop(fds);
            on_frame(fm.serial, fm.buffer_id, fm.width, fm.height, Some(fence), None);
        } else {
            on_frame(fm.serial, fm.buffer_id, fm.width, fm.height, None, None);
        }
        return;
    }
    if fds.is_empty() {
        on_frame(fm.serial, fm.buffer_id, fm.width, fm.height, None, None);
        return;
    }
    let safe_len = safe_mmap_len(&fds[0], size);
    if safe_len == 0 {
        log::warn!(
            "frame fd unusable (fstat failed or empty fd): requested={size}B; continuing with empty data"
        );
        drop(fds);
        on_frame(fm.serial, fm.buffer_id, fm.width, fm.height, None, None);
    } else {
        if safe_len < size {
            log::warn!(
                "frame fd smaller than expected: requested={size}B, actual={safe_len}B (serial={serial}); mapping truncated slice",
                serial = fm.serial
            );
        }
        let pixel = fds.remove(0);
        on_frame(fm.serial, fm.buffer_id, fm.width, fm.height, None, Some(pixel));
        drop(fds);
    }
}

/// F-14: ack a frame (FACK) then, for fence frames, BRDY the slot for reuse.
/// Order is load-bearing: FACK releases the frame's serial server-side; BRDY
/// then re-arms the blit slot (lane 31 gates blitting on it). Legacy
/// (non-fence) frames only FACK — they carry no slot semantics. Verbatim copy
/// host-tested in .omo/start-work/frame-loop-harness (there, the
/// `AppSession::` prefix on write_msg_on is dropped — same free helper).
fn send_fack_and_maybe_brdy(wr: &mut UnixStream, fm: &proto::FrameMessage) -> io::Result<()> {
    let ack = proto::encode(&Message::Ack(proto::FrameAck::new(fm.serial)));
    AppSession::write_msg_on(wr, &ack)?;
    if fm.carries_fence() {
        let brdy = proto::encode(&Message::Ready(proto::BufferReady::new(fm.buffer_id)));
        AppSession::write_msg_on(wr, &brdy)?;
    }
    Ok(())
}

pub struct AppSession {
    pub write_stream: Arc<UnixStream>,
}

impl AppSession {
    /// Connect and return session. The read end is returned separately for
    /// the dedicated recv thread; the write end is shared for JNI calls.
    pub fn connect(path: &str) -> io::Result<(Self, UnixStream)> {
        let stream = UnixStream::connect(path)?;
        stream.set_nonblocking(false)?;
        let write_stream = Arc::new(stream.try_clone()?);
        let read_stream = stream;
        Ok((Self { write_stream }, read_stream))
    }

    pub fn socket_fd(&self) -> std::os::raw::c_int {
        use std::os::fd::AsRawFd;
        self.write_stream.as_raw_fd() as std::os::raw::c_int
    }

    // ── Send (JNI-facing, shared via Arc) ──

    fn write_msg(&self, data: &[u8]) -> io::Result<()> {
        let len = (data.len() as u32).to_le_bytes();
        let mut s = self.write_stream.as_ref();
        s.write_all(&len)?;
        s.write_all(data)?;
        s.flush()
    }

    pub fn send_message(&self, msg: &Message) -> io::Result<()> {
        self.write_msg(&proto::encode(msg))
    }

    pub fn send_config(&self, w: u32, h: u32, refresh_millihz: u32, dpi: u32) -> io::Result<()> {
        let conf = proto::ConfigMessage::new(w, h, refresh_millihz, dpi, 0);
        self.send_message(&Message::Config(conf))
    }

    pub fn send_tbuf(&self, slot: u32, w: u32, h: u32, fmt: u32, stride: u32) -> io::Result<()> {
        let tb = proto::SlotBuffer::new(slot, w, h, fmt, stride);
        self.send_message(&Message::Slot(tb))
    }

    /// F-14: BRDY — signal the server that blit slot `slot` is ready for a
    /// pull-model swap. Lane 29 wires this into the frame loop for pacing;
    /// this lane only provides the primitive. Body mirrors the host-tested
    /// `send_brdy_on` in .omo/start-work/session-brdy-harness.
    pub fn send_brdy(&self, slot: u32) -> io::Result<()> {
        let brdy = proto::BufferReady::new(slot);
        self.send_message(&Message::Ready(brdy))
    }

    // ── Recv (blocking, runs on dedicated thread) ──

    /// `slots` are the blit slot registrations (P-13) built by the CALLER
    /// (lib.rs, lane 30) from render.rs's swapchain images; run_loop stays
    /// decoupled from render/ahb construction and only performs the wire
    /// sends. Empty `slots` is tolerated (warns, stalls blit until lane 30
    /// wires real slots).
    pub fn run_loop(
        read_stream: UnixStream,
        write_stream: UnixStream,
        slots: Vec<AhbSlot>,
        server_caps: Arc<std::sync::atomic::AtomicU32>,
        on_frame: impl Fn(u64, u32, u32, u32, Option<OwnedFd>, Option<OwnedFd>),
    ) -> io::Result<()> {
        dlog("land-native", "run_loop: entered");
        let mut reader = PendingReader::new(read_stream);
        let mut wr = write_stream;

        // 1. Receive HELO
        dlog("land-native", "recv_thread: waiting for HELO...");
        let (data, _fds) = reader.next_message()?;
        let msg = proto::decode(&data, vec![])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        if let Message::Hello(helo) = msg {
            server_caps.store(helo.server_caps, std::sync::atomic::Ordering::Relaxed);
            dlog("land-native", "HELO received");
            log::info!("HELO received (caps={:#x})", helo.server_caps);
        } else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "expected HELO"));
        }

        // 2. Send CONF
        let conf_data = proto::encode(&Message::Config(proto::ConfigMessage::new(3392, 2400, 144000, 289, 0)));
        let len = (conf_data.len() as u32).to_le_bytes();
        wr.write_all(&len)?;
        wr.write_all(&conf_data)?;
        wr.flush()?;
        log::info!("CONF sent, entering frame loop");
        dlog("land-native", "CONF sent, entering frame loop");

        // 2.5 Slot registration (P-13, TODO 28): the server gates frames on
        // SLOT_COUNT TBUFs. For each slot, send the length-prefixed TBUF
        // message and IMMEDIATELY the AHB native_handle on the same socket —
        // sendHandleToUnixSocket's output carries NO u32 length prefix, which
        // matches the server's raw recv_raw. Order is load-bearing: the
        // server decodes TBUF and treats the very next bytes as the handle.
        if slots.is_empty() {
            let msg = "no slots registered — blit mode will stall until lane 30 wires AhbSlots";
            log::warn!("{msg}");
            dlog("land-native", msg);
        } else {
            for slot in &slots {
                let tbuf_data = proto::encode(&slot.to_tbuf_message());
                Self::send_tbuf_then_handle(&mut wr, &tbuf_data, |fd| slot.send_registration(fd))
                    .map_err(|e| {
                        let err_msg = format!("slot registration (slot={}) failed: {e}", slot.slot);
                        dlog("land-native", &err_msg);
                        log::error!("{err_msg}");
                        e
                    })?;
                log::info!(
                    "slot registered: slot={} {}x{} fmt={:#x} stride={}",
                    slot.slot, slot.width, slot.height, slot.format, slot.stride_bytes,
                );
            }
        }

        // 3. Frame ← Ack loop
        loop {
            let (data, fds) = match reader.next_message() {
                Ok(v) => v,
                Err(e) => {
                    let err_msg = format!("next_message failed: {e}");
                    dlog("land-native", &err_msg);
                    log::error!("{err_msg}");
                    return Err(e);
                }
            };
            log::info!("recv: {} bytes, {} fds", data.len(), fds.len());
            let msg = match proto::decode(&data, fds) {
                Ok(m) => m,
                Err(e) => {
                    let err_msg = format!("proto::decode failed: {e}");
                    dlog("land-native", &err_msg);
                    log::error!("{err_msg} data[..min(8)]={:02x?}", &data[..data.len().min(8)]);
                    return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
                }
            };
            match msg {
                Message::Frame(fm, fds) => {
                    log::info!(
                        "Frame received: serial={} {}x{} fds={} fence={}",
                        fm.serial,
                        fm.width,
                        fm.height,
                        fds.len(),
                        fm.carries_fence(),
                    );
                    // P-08/P-08b: dispatch the frame's fds (fence vs pixel
                    // planes) to on_frame — see dispatch_frame for the fence
                    // vs legacy split. on_frame runs synchronously, so the
                    // fence path's present (wired in lib.rs, lane 30) has
                    // completed before the FACK/BRDY below.
                    dispatch_frame(&fm, fds, &on_frame);
                    // F-14: ack the frame (FACK), then — for fence frames,
                    // whose slot semantics make buffer_id == slot — BRDY the
                    // slot for reuse. Order is load-bearing (FACK then BRDY),
                    // host-tested in .omo/start-work/frame-loop-harness.
                    send_fack_and_maybe_brdy(&mut wr, &fm)?;
                }
                other => {
                    log::warn!("unexpected message: {:?}", other);
                }
            }
        }
    }

    /// P-05 framing onto an arbitrary write end (run_loop owns its stream
    /// rather than a session instance): u32 LE length, then the payload.
    fn write_msg_on(wr: &mut UnixStream, data: &[u8]) -> io::Result<()> {
        let len = (data.len() as u32).to_le_bytes();
        wr.write_all(&len)?;
        wr.write_all(data)?;
        wr.flush()
    }

    /// P-13 ordering contract, one slot at a time: the length-prefixed TBUF
    /// message first, then the raw native_handle bytes via `handle_sender`
    /// (production seam: `AhbSlot::send_registration(fd)` →
    /// `AHardwareBuffer_sendHandleToUnixSocket`, whose output has NO length
    /// prefix — it matches the server's raw `recv_raw`). The handle must
    /// never precede the TBUF: the server decodes TBUF and treats the very
    /// next bytes as the handle. Host-tested in
    /// .omo/start-work/session-brdy-harness (verbatim copy).
    fn send_tbuf_then_handle(
        wr: &mut UnixStream,
        tbuf_data: &[u8],
        handle_sender: impl FnOnce(i32) -> Result<(), String>,
    ) -> io::Result<()> {
        Self::write_msg_on(wr, tbuf_data)?;
        handle_sender(wr.as_raw_fd()).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct FdCountGuard {
        before: usize,
    }

    impl FdCountGuard {
        fn new() -> Self {
            Self { before: Self::count() }
        }
        fn count() -> usize {
            fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0)
        }
    }

    impl Drop for FdCountGuard {
        fn drop(&mut self) {
            let after = Self::count();
            assert!(after <= self.before, "fd leak: {0} -> {1}", self.before, after);
        }
    }

    fn memfd_of_size(size: usize) -> OwnedFd {
        let name = CString::new("fstat-guard-test").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create failed: {}", io::Error::last_os_error());
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let rc = unsafe { libc::ftruncate(fd, size as libc::off_t) };
        assert_eq!(rc, 0, "ftruncate failed: {}", io::Error::last_os_error());
        owned
    }

    #[test]
    fn fstat_guard_prevents_oversized_mmap() {
        // 64-byte fd backing a frame claiming width*height*4 = 256 bytes.
        // Mapping 256 bytes past EOF would SIGBUS on read; the guard must
        // clamp the mmap length to the fd's real size (64).
        let _g = FdCountGuard::new();
        let fd = memfd_of_size(64);
        let safe = safe_mmap_len(&fd, 64 * 4);
        assert_eq!(safe, 64, "must clamp to fstat size, not requested size");
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                safe,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED, "mmap of clamped size failed");
        let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, safe) };
        assert_eq!(slice.to_vec().len(), 64);
        unsafe { libc::munmap(ptr, safe); }
    }

    #[test]
    fn fstat_guard_fd_larger_than_requested() {
        let _g = FdCountGuard::new();
        let fd = memfd_of_size(4096);
        assert_eq!(safe_mmap_len(&fd, 256), 256);
    }

    struct InvalidFd;

    impl AsRawFd for InvalidFd {
        fn as_raw_fd(&self) -> std::os::raw::c_int {
            -1
        }
    }

    #[test]
    fn fstat_guard_failure_returns_zero() {
        let _g = FdCountGuard::new();
        assert_eq!(safe_mmap_len(&InvalidFd, 100), 0);
    }

    fn fence_frame(slot: u32) -> proto::FrameMessage {
        let mut fm = proto::FrameMessage {
            magic: proto::MAGIC_LAND,
            num_planes: 1,
            serial: 7,
            modifier: 0,
            width: 64,
            height: 64,
            drm_format: proto::DRM_FORMAT_ABGR8888,
            flags: 0,
            buffer_id: slot,
            _reserved: 0,
            planes: [proto::PlaneDesc { offset: 0, stride: 256 }; 4],
        };
        fm.set_carries_fence(true);
        fm
    }

    /// F-12: a fence frame's LAST fd is handed to on_frame as Some(fence_fd)
    /// (owned — the callback imports it into Vulkan or drops it); no mmap /
    /// pixel data. The callback dropping the fence must not leak the fd.
    #[test]
    fn dispatch_fence_frame_hands_last_fd_as_some() {
        let _g = FdCountGuard::new();
        let fm = fence_frame(2);
        let fence_fd = memfd_of_size(16);
        let fence_raw = fence_fd.as_raw_fd();
        let calls = std::cell::RefCell::new(Vec::new());
        dispatch_frame(&fm, vec![fence_fd], &|serial, buffer_id, w, h, fence, pixels| {
            assert_eq!((serial, buffer_id, w, h), (7, 2, 64, 64));
            assert!(pixels.is_empty(), "fence path must not mmap pixels");
            let f = fence.expect("fence fd must be Some");
            assert_eq!(f.as_raw_fd(), fence_raw, "last fd is the fence");
            calls.borrow_mut().push(f.as_raw_fd());
        });
        assert_eq!(calls.borrow().len(), 1);
    }

    /// P-08b: a frame carrying BOTH planes and a fence splits them — the last
    /// fd is the fence (handed as Some), the plane fds are consumed (dropped
    /// here, not leaked).
    #[test]
    fn dispatch_fence_frame_with_coexisting_planes_drops_planes() {
        let _g = FdCountGuard::new();
        let fm = fence_frame(0);
        let plane = memfd_of_size(16);
        let fence = memfd_of_size(16);
        let fence_raw = fence.as_raw_fd();
        let fence_seen = std::cell::Cell::new(false);
        dispatch_frame(&fm, vec![plane, fence], &|_, _, _, _, f, _| {
            if let Some(fd) = f {
                assert_eq!(fd.as_raw_fd(), fence_raw);
                fence_seen.set(true);
            }
        });
        assert!(fence_seen.get());
    }

    /// P-08: a legacy (non-fence) frame's plane fd is fstat-guarded, mmap'd,
    /// and handed to on_frame as None-fence + the pixel bytes.
    #[test]
    fn dispatch_legacy_frame_mmaps_plane_zero() {
        let _g = FdCountGuard::new();
        let mut fm = fence_frame(3);
        fm.set_carries_fence(false);
        fm.set_carries_fds(true);
        let mut fd = memfd_of_size(64 * 64 * 4);
        {
            let buf = vec![0xABu8; 64 * 64 * 4];
            let rc = unsafe {
                libc::write(
                    fd.as_raw_fd(),
                    buf.as_ptr().cast(),
                    buf.len(),
                )
            };
            assert_eq!(rc, buf.len() as isize);
        }
        let got = std::cell::RefCell::new(Vec::new());
        dispatch_frame(&fm, vec![fd], &|serial, buffer_id, w, h, fence, pixels| {
            assert_eq!((serial, buffer_id, w, h), (7, 3, 64, 64));
            assert!(fence.is_none(), "legacy path has no fence");
            got.borrow_mut().extend_from_slice(pixels);
        });
        assert_eq!(got.borrow().len(), 64 * 64 * 4);
        assert!(got.borrow().iter().all(|&b| b == 0xAB));
    }

    /// F-14: after processing a fence frame, the wire carries FACK then BRDY,
    /// in that order, both decodeable server-side. BRDY's slot == buffer_id.
    #[test]
    fn send_fack_then_brdy_ordering_on_the_wire() {
        let (mut app, mut srv) = UnixStream::pair().unwrap();
        let fm = fence_frame(2);
        send_fack_and_maybe_brdy(&mut app, &fm).expect("send FACK+BRDY");

        let first = read_wire_frame(&mut srv).unwrap();
        match proto::decode(&first, vec![]).expect("decode FACK") {
            Message::Ack(a) => assert_eq!(a.serial, 7),
            other => panic!("expected Ack first, got {other:?}"),
        }
        let second = read_wire_frame(&mut srv).unwrap();
        match proto::decode(&second, vec![]).expect("decode BRDY") {
            Message::Ready(b) => {
                assert_eq!(b.magic, proto::MAGIC_BRDY);
                assert_eq!(b.slot, 2, "BRDY slot must equal buffer_id");
            }
            other => panic!("expected Ready second, got {other:?}"),
        }
    }

    /// F-14: a legacy (non-fence) frame is FACK'd but NOT BRDY'd — it has no
    /// slot semantics; sending BRDY would mis-arm a slot the server blit does
    /// not own.
    #[test]
    fn send_fack_only_for_legacy_frame() {
        let (mut app, mut srv) = UnixStream::pair().unwrap();
        let mut fm = fence_frame(1);
        fm.set_carries_fence(false);
        send_fack_and_maybe_brdy(&mut app, &fm).expect("send FACK");

        let first = read_wire_frame(&mut srv).unwrap();
        match proto::decode(&first, vec![]).expect("decode FACK") {
            Message::Ack(a) => assert_eq!(a.serial, 7),
            other => panic!("expected Ack, got {other:?}"),
        }
        srv.set_nonblocking(true).unwrap();
        let mut probe = [0u8; 4];
        let e = srv.read_exact(&mut probe).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "no BRDY may follow a legacy FACK");
    }

    fn read_wire_frame(srv: &mut UnixStream) -> io::Result<Vec<u8>> {
        use std::io::Read;
        let mut len_buf = [0u8; 4];
        srv.read_exact(&mut len_buf)?;
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut data = vec![0u8; len];
        srv.read_exact(&mut data)?;
        Ok(data)
    }
}

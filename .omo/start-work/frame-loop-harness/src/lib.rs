//! frame-loop-harness — host-testable pure seams for the App frame loop pull
//! model (P3, TODO 29). Verbatim copies of the helpers in
//! android-app/native/src/session.rs; if they diverge, this harness is lying.
//!
//! What is tested here:
//!   - F-12/P-08b: `dispatch_frame` splits a frame's SCM_RIGHTS fds — the LAST
//!     fd is the sync_file blit fence (handed to on_frame as `Some(fence_fd)`,
//!     ownership transferred: the callback imports it into Vulkan or drops it),
//!     and any coexisting pixel-plane fds are consumed (dropped, not leaked).
//!   - P-08: the legacy (non-fence) SHM path mmaps `fds[0]` and hands pixels
//!     with `fence_fd = None` — unchanged transitional behavior.
//!   - F-14: after processing a fence frame the wire carries FACK then BRDY
//!     (order load-bearing); a legacy frame is FACK'd but NOT BRDY'd.
//!
//! The only divergence from session.rs is context: session.rs's helpers call
//! `AppSession::write_msg_on` (an associated fn); here it is the free
//! `write_msg_on` below. Bodies are otherwise identical.

// Verbatim session.rs helpers are consumed only by this crate's tests.
#![allow(dead_code)]

use std::io::{self, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use wl_android_common::proto::{self, Message};

// ============================================================================
// Verbatim pure helpers from session.rs (keep in sync!)
// ============================================================================

/// Length-prefix + payload on `wr` (the P-05 wire framing: u32 LE length
/// followed by the message body). session.rs has this as
/// `AppSession::write_msg_on`; the body is identical.
fn write_msg_on(wr: &mut UnixStream, data: &[u8]) -> io::Result<()> {
    let len = (data.len() as u32).to_le_bytes();
    wr.write_all(&len)?;
    wr.write_all(data)?;
    wr.flush()
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

/// P-08/P-08b fd split + dispatch. FENCE path (carries_fence): the LAST fd is
/// the sync_file blit fence (app_link.rs send_frame pushes pixel fds first,
/// then the fence; proto::fd_count counts planes then +1 fence). The fence is
/// handed to on_frame as `Some(fence_fd)` (ownership transfers — F-12); any
/// coexisting plane fds are dropped (the blit already landed in the slot, the
/// App consumes no pixel bytes). LEGACY path (no fence): `fds[0]` is the pixel
/// dmabuf, fstat-guarded and mmap'd; on_frame gets `fence_fd = None` + pixels.
fn dispatch_frame(
    fm: &proto::FrameMessage,
    fds: Vec<OwnedFd>,
    on_frame: &impl Fn(u64, u32, u32, u32, Option<OwnedFd>, &[u8]),
) {
    let size = fm.width as usize * fm.height as usize * 4;
    let mut fds = fds;
    if fm.carries_fence() {
        // Fence is the last fd (P-08b). decode() enforced fd_count, so pop()
        // yields the fence in practice; plane fds (if any) are dropped below.
        if let Some(fence) = fds.pop() {
            drop(fds);
            on_frame(fm.serial, fm.buffer_id, fm.width, fm.height, Some(fence), &[]);
        } else {
            on_frame(fm.serial, fm.buffer_id, fm.width, fm.height, None, &[]);
        }
        return;
    }
    if fds.is_empty() {
        on_frame(fm.serial, fm.buffer_id, fm.width, fm.height, None, &[]);
        return;
    }
    let safe_len = safe_mmap_len(&fds[0], size);
    if safe_len == 0 {
        log::warn!(
            "frame fd unusable (fstat failed or empty fd): requested={size}B; continuing with empty data"
        );
        drop(fds);
        on_frame(fm.serial, fm.buffer_id, fm.width, fm.height, None, &[]);
    } else {
        if safe_len < size {
            log::warn!(
                "frame fd smaller than expected: requested={size}B, actual={safe_len}B (serial={serial}); mapping truncated slice",
                serial = fm.serial
            );
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                safe_len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fds[0].as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            drop(fds);
            on_frame(fm.serial, fm.buffer_id, fm.width, fm.height, None, &[]);
        } else {
            // SAFETY: ptr is a live, readable mapping of safe_len bytes
            // (fstat-guarded); it stays alive through on_frame and is
            // released right after the callback returns. on_frame must not
            // retain the slice past the call (run_loop CONTRACT).
            let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, safe_len) };
            on_frame(fm.serial, fm.buffer_id, fm.width, fm.height, None, slice);
            unsafe { libc::munmap(ptr, safe_len); }
            drop(fds);
        }
    }
}

/// F-14: ack a frame (FACK) then, for fence frames, BRDY the slot for reuse.
/// Order is load-bearing: FACK releases the frame's serial server-side; BRDY
/// then re-arms the blit slot (lane 31 gates blitting on it). Legacy
/// (non-fence) frames only FACK — they carry no slot semantics.
fn send_fack_and_maybe_brdy(wr: &mut UnixStream, fm: &proto::FrameMessage) -> io::Result<()> {
    let ack = proto::encode(&Message::Ack(proto::FrameAck::new(fm.serial)));
    write_msg_on(wr, &ack)?;
    if fm.carries_fence() {
        let brdy = proto::encode(&Message::Ready(proto::BufferReady::new(fm.buffer_id)));
        write_msg_on(wr, &brdy)?;
    }
    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::FromRawFd;

    // /proc/self/fd is process-global, so FdCountGuard is only deterministic
    // while no other fd-touching test runs concurrently. Same convention as the
    // crate (frame_cache.rs FD_GUARD_LOCK): a static mutex serializes the
    // guard-bearing tests so they are mutually exclusive even under parallel
    // `cargo test`. Tests not using the guard stay parallel.
    static FD_GUARD_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

    fn fd_guard_lock() -> &'static std::sync::Mutex<()> {
        FD_GUARD_LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    struct FdCountGuard {
        before: usize,
    }

    impl FdCountGuard {
        fn new() -> Self {
            Self { before: Self::count() }
        }
        fn count() -> usize {
            std::fs::read_dir("/proc/self/fd")
                .map(|d| d.count())
                .unwrap_or(0)
        }
    }

    impl Drop for FdCountGuard {
        fn drop(&mut self) {
            let after = Self::count();
            assert!(
                after <= self.before,
                "fd leak: {0} -> {1}",
                self.before,
                after
            );
        }
    }

    fn memfd_of_size(size: usize) -> OwnedFd {
        let name = std::ffi::CString::new("frame-loop-test").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create failed: {}", io::Error::last_os_error());
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let rc = unsafe { libc::ftruncate(fd, size as libc::off_t) };
        assert_eq!(rc, 0, "ftruncate failed: {}", io::Error::last_os_error());
        owned
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

    fn read_wire_frame(srv: &mut UnixStream) -> io::Result<Vec<u8>> {
        // Read timeout so a missing message fails the test instead of hanging.
        srv.set_read_timeout(Some(std::time::Duration::from_millis(3000)))?;
        let mut len_buf = [0u8; 4];
        srv.read_exact(&mut len_buf)?;
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut data = vec![0u8; len];
        srv.read_exact(&mut data)?;
        Ok(data)
    }

    /// F-12: a fence frame's LAST fd is handed to on_frame as Some(fence_fd)
    /// (owned — the callback imports it into Vulkan or drops it); no mmap /
    /// pixel data. Dropping the fence must not leak the fd.
    #[test]
    fn dispatch_fence_frame_hands_last_fd_as_some() {
        let _serial = fd_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
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
        let _serial = fd_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
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
        let _serial = fd_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _g = FdCountGuard::new();
        let mut fm = fence_frame(3);
        fm.set_carries_fence(false);
        fm.set_carries_fds(true);
        let fd = memfd_of_size(64 * 64 * 4);
        let buf = vec![0xABu8; 64 * 64 * 4];
        let rc = unsafe { libc::write(fd.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
        assert_eq!(rc, buf.len() as isize);
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
        let _serial = fd_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
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
        let _serial = fd_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
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
        assert_eq!(
            e.kind(),
            io::ErrorKind::WouldBlock,
            "no BRDY may follow a legacy FACK"
        );
    }
}

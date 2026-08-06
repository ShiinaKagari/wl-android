use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use wl_android_common::proto;
use wl_android_common::proto::Message;


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

/// Split a received Frame's fds per the wire contract (P-08) and route the
/// frame to `on_frame`. SHM-only protocol: `fds[0]` is the pixel fd. It is
/// fstat-guarded and handed to `on_frame` by ownership (no mmap here) — the
/// recv thread enqueues it and the render thread does the mmap+copy. A
/// truncated frame is reported instead of SIGBUSing in the render thread.
fn dispatch_frame(
    fm: &proto::FrameMessage,
    fds: Vec<OwnedFd>,
    on_frame: &impl Fn(u64, u32, u32, u32, Option<OwnedFd>, Option<OwnedFd>),
) {
    let size = fm.width as usize * fm.height as usize * 4;
    let mut fds = fds;
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


/// F-14: ack a frame (FACK). SHM-only protocol — frames carry no slot
/// semantics, so no BRDY follows the FACK.
fn send_fack(wr: &mut UnixStream, fm: &proto::FrameMessage) -> io::Result<()> {
    let ack = proto::encode(&Message::Ack(proto::FrameAck::new(fm.serial)));
    AppSession::write_msg_on(wr, &ack)
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

    pub fn send_config(&self, w: u32, h: u32, refresh_millihz: u32, dpi: u32, frame_mode: u32) -> io::Result<()> {
        let conf = proto::ConfigMessage::new(w, h, refresh_millihz, dpi, 0, frame_mode);
        self.send_message(&Message::Config(conf))
    }

    // ── Recv (blocking, runs on dedicated thread) ──

    pub fn run_loop(
        read_stream: UnixStream,
        write_stream: UnixStream,
        server_caps: Arc<std::sync::atomic::AtomicU32>,
        on_connected: impl FnOnce(),
        on_frame: impl Fn(u64, u32, u32, u32, Option<OwnedFd>, Option<OwnedFd>),
        on_config_update: impl Fn(u32, u32, u32, u32, u32),
    ) -> io::Result<()> {
        log::debug!("run_loop: entered");
        let mut reader = PendingReader::new(read_stream);
        let mut wr = write_stream;

        // 1. Receive HELO
        log::debug!("recv_thread: waiting for HELO...");
        let (data, _fds) = reader.next_message()?;
        let msg = proto::decode(&data, vec![])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        if let Message::Hello(helo) = msg {
            server_caps.store(helo.server_caps, std::sync::atomic::Ordering::Relaxed);
            log::info!("HELO received (caps={:#x})", helo.server_caps);
        } else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "expected HELO"));
        }

        // 2. Send CONF
        let conf_data = proto::encode(&Message::Config(proto::ConfigMessage::new(3392, 2400, 144000, 289, 0, 0)));
        let len = (conf_data.len() as u32).to_le_bytes();
        wr.write_all(&len)?;
        wr.write_all(&conf_data)?;
        wr.flush()?;
        log::info!("CONF sent, entering frame loop");
        // CONN-STATE: the handshake is complete — the session is live even
        // before any frame arrives (KWin may be idle). Mark Active so the
        // status overlay hides immediately on reconnect.
        on_connected();

        // 3. Frame ← Ack loop
        loop {
            let (data, fds) = match reader.next_message() {
                Ok(v) => v,
                Err(e) => {
                    let err_msg = format!("next_message failed: {e}");
                    log::error!("{err_msg}");
                    return Err(e);
                }
            };
            log::debug!("recv: {} bytes, {} fds", data.len(), fds.len());
            let msg = match proto::decode(&data, fds) {
                Ok(m) => m,
                Err(e) => {
                    let err_msg = format!("proto::decode failed: {e}");
                    log::error!("{err_msg} data[..min(8)]={:02x?}", &data[..data.len().min(8)]);
                    return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
                }
            };
            match msg {
                Message::Frame(fm, fds) => {
                    log::debug!(
                        "Frame received: serial={} {}x{} fds={}",
                        fm.serial,
                        fm.width,
                        fm.height,
                        fds.len(),
                    );
                    // P-08: dispatch the frame's pixel fd to on_frame — see
                    // dispatch_frame. on_frame runs synchronously, so the
                    // CPU-present (wired in lib.rs, render thread) completes
                    // before the FACK below.
                    dispatch_frame(&fm, fds, &on_frame);
                    // F-14: ack the frame (FACK). SHM-only protocol — no BRDY.
                    send_fack(&mut wr, &fm)?;
                }
                Message::ConfigUpdate(c) => {
                    log::info!(
                        "ConfigUpdate received: {}x{} @{}mHz dpi={} mode={}",
                        c.width, c.height, c.refresh_millihz, c.dpi, c.frame_mode,
                    );
                    on_config_update(c.width, c.height, c.refresh_millihz, c.dpi, c.frame_mode);
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

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
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

    /// F-14: an SHM frame is FACK'd and nothing else follows — it has no slot
    /// semantics, so no BRDY may appear on the wire after the FACK.
    #[test]
    fn send_fack_only_for_shm_frame() {
        let (mut app, mut srv) = UnixStream::pair().unwrap();
        let fm = shm_frame(1);
        send_fack(&mut app, &fm).expect("send FACK");

        let first = read_wire_frame(&mut srv).unwrap();
        match proto::decode(&first, vec![]).expect("decode FACK") {
            Message::Ack(a) => assert_eq!(a.serial, 7),
            other => panic!("expected Ack, got {other:?}"),
        }
        srv.set_nonblocking(true).unwrap();
        let mut probe = [0u8; 4];
        let e = srv.read_exact(&mut probe).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "no BRDY may follow an SHM FACK");
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

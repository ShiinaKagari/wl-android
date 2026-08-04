use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use nix::sys::socket::{self, MsgFlags, ControlMessage, ControlMessageOwned};
use wl_android_common::proto::{self, Message};

/// Max fds a single recvmsg must be able to carry. The App registers all its
/// slots back-to-back (5 TBUF+handle pairs → 5 SCM_RIGHTS fds coalescing into
/// one recvmsg); a frame may add a fence fd on top. cmsg space is ~24 B/fd, so
/// 16 is cheap headroom. (`cmsg_space!([RawFd; 4])` truncated the burst —
/// MSG_CTRUNC silently dropped the excess fds.)
const MAX_RECV_FDS: usize = 16;

/// Message transport over a SOCK_STREAM unix socket.
///
/// DESIGN.md P-01 mandates SOCK_SEQPACKET so the kernel preserves message
/// boundaries and keeps each SCM_RIGHTS fd aligned with the bytes it was sent
/// with. The protocol deviates and uses SOCK_STREAM + a u32 length prefix,
/// which lets multiple messages (and their fds) coalesce into one recvmsg.
/// This transport compensates with a persistent read-ahead buffer
/// (`pending` plus `pending_fds`); the SEQPACKET deviation remains a known
/// deviation (DESIGN.md P-01) but the buffering below makes it safe.
pub struct Transport {
    stream: UnixStream,
    /// Scratch read buffer for a single recvmsg (up to 64 KiB).
    recv_buf: Vec<u8>,
    /// Bytes read from the socket but not yet consumed by a message or raw
    /// chunk. Absorbs coalesced trailing bytes (e.g. a native_handle that
    /// followed a TBUF in the same recvmsg) so nothing is silently dropped.
    pending: Vec<u8>,
    /// SCM_RIGHTS fds that arrived together with `pending`'s bytes.
    ///
    /// FD-ORDERING RULE: on SOCK_STREAM the kernel delivers fds in the order
    /// their bytes were sent, and this transport consumes messages in byte
    /// order, so fds are consumed in order: the first N fds (N = fd count of
    /// the decoded message, or all of them for a raw chunk) belong to the
    /// current message and the remainder stays pending for the next one. This
    /// holds for the TBUF + native_handle pattern: the TBUF carries 0 fds and
    /// the handle carries exactly its own fds, so after a coalesced read the
    /// TBUF consumes 0 fds and the handle's fds are carried over intact.
    ///
    /// `pending`/`pending_fds` are per-session (one Transport per AppSession)
    /// and are dropped together with the Transport on disconnect — no explicit
    /// clearing is needed.
    pending_fds: Vec<OwnedFd>,
}

impl Transport {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            recv_buf: vec![0u8; 65536],
            pending: Vec::new(),
            pending_fds: Vec::new(),
        })
    }

    pub fn send(&mut self, msg: &Message) -> io::Result<()> {
        let bytes = proto::encode(msg);
        let len_bytes = (bytes.len() as u32).to_le_bytes();

        let fds: Vec<RawFd> = match msg {
            Message::Frame(_, fds) if proto::fd_count(msg) > 0 => {
                fds.iter().map(|fd| fd.as_raw_fd()).collect()
            }
            _ => vec![],
        };

        let iov = [
            std::io::IoSlice::new(&len_bytes),
            std::io::IoSlice::new(&bytes),
        ];

        let cmsgs = if !fds.is_empty() {
            vec![ControlMessage::ScmRights(&fds)]
        } else {
            vec![]
        };

        socket::sendmsg::<()>(
            self.stream.as_raw_fd(),
            &iov,
            &cmsgs,
            MsgFlags::empty(),
            None,
        )?;
        Ok(())
    }

    /// Receive one length-prefixed message (non-blocking; Ok(None) on EAGAIN).
    ///
    /// Serves a complete message from `pending` first (never blocks); only when
    /// `pending` holds no complete message does it issue a new recvmsg, whose
    /// result is appended to `pending` — trailing bytes + their fds stay
    /// buffered for the next call instead of being dropped.
    pub fn recv(&mut self) -> io::Result<Option<Message>> {
        if let Some((body, fds)) = self.take_pending_message()? {
            return self.decode_msg(&body, fds).map(Some);
        }

        let (data, fds) = match self.recv_raw_inner(MsgFlags::MSG_DONTWAIT) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        };
        if data.is_empty() {
            // Peer closed / nothing queued; `pending` (partial or raw bytes)
            // is left as-is for a later call.
            return Ok(None);
        }

        self.pending.extend_from_slice(&data);
        self.pending_fds.extend(fds);

        match self.take_pending_message()? {
            Some((body, fds)) => self.decode_msg(&body, fds).map(Some),
            None => Ok(None), // partial length prefix — wait for the rest
        }
    }

    /// Receive raw bytes + fds without length prefix (for native_handle, P-13).
    /// Returns None on EAGAIN. Serves `pending` first; only when `pending` is
    /// empty does it issue a new recvmsg, so each call returns at most one
    /// recvmsg's worth of bytes. A caller needing a full native_handle must
    /// poll (as app_link::recv_native_handle_follow_up does); in practice a
    /// 12–32 B handle arrives whole in one recvmsg.
    pub fn recv_raw(&mut self) -> io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
        if !self.pending.is_empty() {
            // Per the FD-ORDERING RULE, every fd that was not consumed by the
            // length-prefixed message belongs to this trailing raw chunk.
            let data = mem::take(&mut self.pending);
            let fds = mem::take(&mut self.pending_fds);
            return Ok(Some((data, fds)));
        }

        match self.recv_raw_inner(MsgFlags::MSG_DONTWAIT) {
            Ok((d, f)) => {
                if d.is_empty() { Ok(None) } else { Ok(Some((d, f))) }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Raw receive that waits (bounded) until `min_fds` SCM_RIGHTS fds are
    /// buffered, then returns the pending blob (bytes + fds). On SOCK_STREAM
    /// the kernel can deliver a native_handle's bytes in one recvmsg and its
    /// fds in a later one, so the P-13 consumer must not give up when the
    /// first raw chunk comes up fd-short — it keeps pulling recvmsgs into
    /// `pending` until the trailing fds arrive (or `timeout` elapses).
    ///
    /// With `min_fds == 0` it serves the next byte-carrying recvmsg instead
    /// (used to accumulate a split native_handle header). Never returns an
    /// empty blob: Ok(None) means the socket closed or the timeout expired.
    pub fn recv_raw_with_fd_wait(
        &mut self,
        min_fds: usize,
        timeout: std::time::Duration,
    ) -> io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            // With min_fds > 0, serve once that many fds are buffered (bytes
            // ride along); with min_fds == 0, serve the next byte-carrying
            // recvmsg (used to accumulate a split native_handle header).
            let serve_fds = min_fds > 0 && self.pending_fds.len() >= min_fds;
            let serve_bytes = !self.pending.is_empty();
            if serve_fds || (min_fds == 0 && serve_bytes) {
                let data = mem::take(&mut self.pending);
                let fds = mem::take(&mut self.pending_fds);
                return Ok(Some((data, fds)));
            }
            match self.recv_raw_inner(MsgFlags::MSG_DONTWAIT) {
                Ok((data, fds)) => {
                    if data.is_empty() && fds.is_empty() {
                        return Ok(None); // peer closed
                    }
                    self.pending.extend_from_slice(&data);
                    self.pending_fds.extend(fds);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return Ok(None);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Put raw bytes + fds back at the FRONT of `pending`. The P-13 consumer
    /// uses this to preserve whatever followed a native_handle inside the same
    /// coalesced recvmsg (e.g. the next [len][TBUF][handle] unit) so the next
    /// recv()/recv_raw() sees it instead of it being silently dropped.
    pub fn unrecv_raw(&mut self, data: Vec<u8>, fds: Vec<OwnedFd>) {
        if !data.is_empty() {
            let mut combined = data;
            combined.extend_from_slice(&self.pending);
            self.pending = combined;
        }
        if !fds.is_empty() {
            let mut combined = fds;
            combined.append(&mut self.pending_fds);
            self.pending_fds = combined;
        }
    }

    /// Send raw bytes + fds (no length prefix). Used for native_handle forwarding.
    #[allow(dead_code)]
    pub fn send_raw(&mut self, data: &[u8], fds: &[RawFd]) -> io::Result<()> {
        let iov = [std::io::IoSlice::new(data)];
        let cmsgs = if !fds.is_empty() {
            vec![ControlMessage::ScmRights(fds)]
        } else {
            vec![]
        };
        socket::sendmsg::<()>(
            self.stream.as_raw_fd(),
            &iov,
            &cmsgs,
            MsgFlags::empty(),
            None,
        )?;
        Ok(())
    }

    /// If `pending` holds a complete length-prefixed message, extract its body
    /// and the fds aligned to it (FD-ORDERING RULE), leaving any trailing bytes
    /// + fds in place. Ok(None) for a partial message.
    fn take_pending_message(&mut self) -> io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
        if self.pending.len() < 4 {
            return Ok(None);
        }
        let msg_len = u32::from_le_bytes([
            self.pending[0],
            self.pending[1],
            self.pending[2],
            self.pending[3],
        ]) as usize;
        // All protocol messages are <= 80 B; guard against a garbage length
        // prefix (e.g. native_handle bytes misread as a length) so a desynced
        // stream fails fast instead of stalling forever.
        const MAX_MSG_LEN: usize = 1 << 20;
        if msg_len > MAX_MSG_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unreasonable message length: {msg_len}"),
            ));
        }
        let total = 4 + msg_len;
        if self.pending.len() < total {
            return Ok(None);
        }

        let body = self.pending[4..total].to_vec();
        self.pending.drain(..total);

        let need = Self::msg_fd_count(&body);
        let take = need.min(self.pending_fds.len());
        let fds: Vec<OwnedFd> = self.pending_fds.drain(..take).collect();
        Ok(Some((body, fds)))
    }

    fn decode_msg(&self, body: &[u8], fds: Vec<OwnedFd>) -> io::Result<Message> {
        proto::decode(body, fds)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }

    /// Number of SCM_RIGHTS fds a length-prefixed message body consumes. Only
    /// Frame messages carry fds (P-08/P-08b); mirrors proto::fd_count on raw
    /// body bytes so the transport can align fds to a message before decode.
    fn msg_fd_count(body: &[u8]) -> usize {
        if body.len() < 4 {
            return 0;
        }
        let magic = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        // FrameMessage layout: magic@0, num_planes@4, ..., flags@36.
        if magic != proto::MAGIC_LAND || body.len() < 40 {
            return 0;
        }
        let num_planes = u32::from_le_bytes([body[4], body[5], body[6], body[7]]) as usize;
        let flags = u32::from_le_bytes([body[36], body[37], body[38], body[39]]);
        (if flags & proto::FRAME_CARRIES_FDS != 0 { num_planes } else { 0 })
            + (if flags & proto::FRAME_CARRIES_FENCE != 0 { 1 } else { 0 })
    }

    fn recv_raw_inner(&mut self, flags: MsgFlags) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
        use std::io::IoSliceMut;
        // One recvmsg can coalesce the whole slot-registration burst: the App
        // sends all TBUF+handle pairs back-to-back, so all 5 SCM_RIGHTS fds
        // land in a single cmsg block (a frame may add a fence fd on top). The
        // old `[RawFd; 4]` truncated anything with 5 fds (MSG_CTRUNC drops the
        // excess), and 16 fds cost only ~400 B of cmsg space.
        let mut cmsg_space = nix::cmsg_space!([RawFd; MAX_RECV_FDS]);

        let (n_bytes, fds) = {
            let mut iov = [IoSliceMut::new(&mut self.recv_buf)];
            let recv_msg = socket::recvmsg::<()>(
                self.stream.as_raw_fd(),
                &mut iov,
                Some(&mut cmsg_space),
                flags,
            )?;

            let n = recv_msg.bytes;
            let fds: Vec<OwnedFd> = match recv_msg.cmsgs() {
                Ok(cmsgs) => cmsgs
                    .flat_map(|cmsg| match cmsg {
                        ControlMessageOwned::ScmRights(fds) => fds,
                        _ => vec![],
                    })
                    .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
                    .collect(),
                Err(_) => vec![],
            };
            (n, fds)
        };

        let data = self.recv_buf[..n_bytes].to_vec();
        Ok((data, fds))
    }
}

impl AsRawFd for Transport {
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use wl_android_common::proto::HelloMessage;

    // X-04: fd leak guard — /proc/self/fd is process-global, so guard tests are
    // only deterministic while no other fd-touching test runs concurrently.
    // Combine with `--test-threads=1` (the crate's established convention).
    static FD_GUARD_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

    fn fd_guard_lock() -> &'static std::sync::Mutex<()> {
        FD_GUARD_LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    struct FdCountGuard {
        initial: usize,
        label: &'static str,
    }

    impl FdCountGuard {
        fn new(label: &'static str) -> Self {
            let initial = std::fs::read_dir("/proc/self/fd")
                .map(|entries| entries.count())
                .unwrap_or(0);
            Self { initial, label }
        }
    }

    impl Drop for FdCountGuard {
        fn drop(&mut self) {
            let current = std::fs::read_dir("/proc/self/fd")
                .map(|entries| entries.count())
                .unwrap_or(0);
            assert_eq!(
                current, self.initial,
                "FdCountGuard [{}]: fd count changed: {} != {} (leak detected)",
                self.label, current, self.initial
            );
        }
    }

    fn socketpair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("socketpair")
    }

    fn fake_dmabuf_fd() -> OwnedFd {
        socketpair().0.into()
    }

    // GraphicBuffer::flatten wire format (what AHardwareBuffer_sendHandleToUnixSocket
    // actually sends): [GBFR magic][w][h][format][layers][usageLo][usageHi][stride],
    // 32 bytes fixed header; fds travel as SCM_RIGHTS. numFds=1 for one image.
    fn native_handle_bytes() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&crate::ahb_handle::FLAT_MAGIC.to_le_bytes()); // 'GBFR'
        data.extend_from_slice(&100i32.to_le_bytes()); // width
        data.extend_from_slice(&100i32.to_le_bytes()); // height
        data.extend_from_slice(&1i32.to_le_bytes());   // format RGBA_8888
        data.extend_from_slice(&1i32.to_le_bytes());   // layerCount
        data.extend_from_slice(&0x100u32.to_le_bytes()); // usageLo GPU_SAMPLED
        data.extend_from_slice(&0i32.to_le_bytes());   // usageHi
        data.extend_from_slice(&400i32.to_le_bytes()); // stride (pixels)
        data
    }

    fn poll_recv(t: &mut Transport) -> Message {
        for _ in 0..100 {
            if let Some(m) = t.recv().unwrap() {
                return m;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("recv timed out");
    }

    fn poll_recv_raw(t: &mut Transport) -> (Vec<u8>, Vec<OwnedFd>) {
        for _ in 0..100 {
            if let Some(r) = t.recv_raw().unwrap() {
                return r;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("recv_raw timed out");
    }

    /// The received fd must be a live descriptor: getsockopt succeeds and it is
    /// a stream socket (the fake dmabuf we sent is a socketpair endpoint, so a
    /// dup through SCM_RIGHTS must still be a socket).
    fn assert_valid_fd(fd: &OwnedFd) {
        let t = nix::sys::socket::getsockopt(fd, nix::sys::socket::sockopt::SockType)
            .expect("received fd must be a valid live descriptor");
        assert_eq!(t, nix::sys::socket::SockType::Stream);
    }

    #[test]
    fn send_recv_hello_roundtrip() {
        let (srv, cli) = socketpair();
        let mut transport = Transport::new(srv).unwrap();
        let mut reader = Transport::new(cli).unwrap();

        let helo = HelloMessage::default();
        transport.send(&Message::Hello(helo)).unwrap();

        // Non-blocking recv: data may arrive asynchronously
        let mut attempts = 0;
        let received = loop {
            match reader.recv().unwrap() {
                Some(msg) => break msg,
                None => {
                    attempts += 1;
                    if attempts > 100 {
                        panic!("recv timed out");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        };
        assert!(matches!(received, Message::Hello(_)));
    }

    // THE regression test (mirrors the real-device failure): the App sends the
    // TBUF length-prefixed message and the native_handle raw payload back-to-back
    // with NO delay. On SOCK_STREAM both byte-ranges + the SCM_RIGHTS fd coalesce
    // into one recvmsg. The pre-fix transport consumed only the TBUF and silently
    // dropped the trailing handle bytes + its fd, so recv_raw() never saw them.
    #[test]
    fn regression_tbuf_plus_native_handle_back_to_back() {
        // Ignore poisoning: the lock only serializes fd-touching tests (a prior
        // RED-phase panic must not cascade into PoisonError for the next test).
        let _lock = fd_guard_lock().lock().unwrap_or_else(|p| p.into_inner());
        let _guard = FdCountGuard::new("regression-tbuf-plus-handle");

        let (srv, cli) = socketpair();
        let mut server = Transport::new(srv).unwrap();
        let mut client = Transport::new(cli).unwrap();

        let fake = fake_dmabuf_fd();
        let raw_fd = fake.as_raw_fd();

        // (a) length-prefixed TBUF, then (b) raw native_handle — back-to-back, no delay.
        client
            .send(&Message::Slot(proto::SlotBuffer::new(
                0, 100, 100, proto::DRM_FORMAT_ABGR8888, 400,
            )))
            .unwrap();
        client.send_raw(&native_handle_bytes(), &[raw_fd]).unwrap();

        // Server recv() must return the Slot...
        let msg = poll_recv(&mut server);
        assert!(matches!(msg, Message::Slot(s) if s.slot == 0));

        // ...then recv_raw() must return the handle bytes + its fd (not the next
        // message and not garbage).
        let (data, fds) = poll_recv_raw(&mut server);
        assert_eq!(data, native_handle_bytes());
        assert_eq!(fds.len(), 1);
        assert_valid_fd(&fds[0]);
    }

    // P-05 coalescing: two length-prefixed messages sent back-to-back must both
    // be delivered, in order, with no bytes lost.
    #[test]
    fn recv_consumes_only_one_message_keeps_rest() {
        let (srv, cli) = socketpair();
        let mut server = Transport::new(srv).unwrap();
        let mut client = Transport::new(cli).unwrap();

        let touch = Message::Touch(proto::TouchMessage::new(
            1, 10.0, 20.0, proto::TOUCH_PHASE_DOWN, 5,
        ));
        let conf = Message::Config(proto::ConfigMessage::new(
            800, 600, 60000, 96, proto::APP_CAP_DIRECT_IMPORT,
        ));

        client.send(&touch).unwrap();
        client.send(&conf).unwrap();

        assert_eq!(poll_recv(&mut server), touch);
        assert_eq!(poll_recv(&mut server), conf);
    }

    // P-13 + X-04: a length-prefixed Hello (0 fds) coalesced with a raw
    // native_handle carrying one fd. The first recv() consumes 0 fds; the fd must
    // survive into the trailing raw chunk so recv_raw() returns it intact.
    #[test]
    fn recv_raw_after_recv_gets_trailing_fd() {
        // Ignore poisoning: the lock only serializes fd-touching tests (a prior
        // RED-phase panic must not cascade into PoisonError for the next test).
        let _lock = fd_guard_lock().lock().unwrap_or_else(|p| p.into_inner());
        let _guard = FdCountGuard::new("recv-raw-after-recv-trailing-fd");

        let (srv, cli) = socketpair();
        let mut server = Transport::new(srv).unwrap();
        let mut client = Transport::new(cli).unwrap();

        let fake = fake_dmabuf_fd();
        let raw_fd = fake.as_raw_fd();

        client
            .send(&Message::Hello(HelloMessage::default()))
            .unwrap();
        client.send_raw(&native_handle_bytes(), &[raw_fd]).unwrap();

        let msg = poll_recv(&mut server);
        assert!(matches!(msg, Message::Hello(_)));

        let (data, fds) = poll_recv_raw(&mut server);
        assert_eq!(data, native_handle_bytes());
        assert_eq!(fds.len(), 1);
        assert_valid_fd(&fds[0]);
    }

    // THE device-verified blocker (multi-slot registration burst). The real App
    // registers ALL its slots back-to-back in the same millisecond; the 5
    // TBUF+handle pairs coalesce into ONE recvmsg carrying 5 SCM_RIGHTS fds.
    // The pre-fix transport had cmsg room for only 4 fds (MSG_CTRUNC dropped
    // the 5th) and recv_raw() drained the WHOLE coalesced blob — it consumed
    // handle0 but silently discarded the trailing [len][TBUF1..4][handle1..4]
    // bytes + their fds, so slot 0 registered and then the session desynced
    // (the App's remaining TBUFs never arrived → stall → ECONNRESET).
    #[test]
    fn slot_registration_five_pair_burst_tight_loop() {
        use crate::app_link::AppSession;
        use crate::blit::BlitEngine;

        let _lock = fd_guard_lock().lock().unwrap_or_else(|p| p.into_inner());
        let _guard = FdCountGuard::new("slot-burst-5-pair");

        let (srv, cli) = socketpair();
        let mut session = AppSession::new(Transport::new(srv).unwrap());
        let mut client = Transport::new(cli).unwrap();
        let mut blit = BlitEngine::new(); // uninitialized → import errors, flow continues

        let fake_fds: Vec<OwnedFd> = (0..5).map(|_| fake_dmabuf_fd()).collect();
        let raw_fds: Vec<RawFd> = fake_fds.iter().map(|fd| fd.as_raw_fd()).collect();

        let handle = std::thread::spawn(move || {
            loop {
                match client.recv().unwrap() {
                    Some(Message::Hello(_)) => break,
                    None => std::thread::sleep(std::time::Duration::from_millis(5)),
                    _ => {}
                }
            }
            client
                .send(&Message::Config(proto::ConfigMessage::new(
                    100, 100, 60000, 96, 0,
                )))
                .unwrap();

            // The device burst: ALL pairs in one write, 5 fds coalescing into a
            // single server recvmsg. Interleaved wire:
            // [len][TBUF0][handle0][len][TBUF1][handle1] ... [len][TBUF4][handle4]
            let mut wire = Vec::new();
            for slot in 0..5u32 {
                let tb = proto::encode(&Message::Slot(proto::SlotBuffer::new(
                    slot, 100, 100, proto::DRM_FORMAT_ABGR8888, 400,
                )));
                wire.extend_from_slice(&(tb.len() as u32).to_le_bytes());
                wire.extend_from_slice(&tb);
                wire.extend_from_slice(&native_handle_bytes());
            }
            client.send_raw(&wire, &raw_fds).unwrap();
        });

        loop {
            match session.do_handshake().unwrap() {
                true => break,
                false => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }

        // All 5 TBUFs must be delivered in order, each followed by a parseable
        // native_handle. The pre-fix transport stalls at slot 1 (trailing
        // handles were dropped), so this loop times out → RED.
        for slot in 0..5u32 {
            let mut attempts = 0;
            let msg = loop {
                attempts += 1;
                if attempts > 400 {
                    panic!("burst: slot {slot} never arrived — transport dropped coalesced trailing handles");
                }
                match session.recv_message(&mut blit).unwrap() {
                    Some(m) => break m,
                    None => std::thread::sleep(std::time::Duration::from_millis(5)),
                }
            };
            assert!(
                matches!(msg, Message::Slot(s) if s.slot == slot),
                "slots delivered out of order: expected {slot}, got {msg:?}"
            );
        }
        assert_eq!(session.slot_count(), 5);

        handle.join().unwrap();
    }

    // fd-bytes misalignment on SOCK_STREAM (root cause #2): the kernel can
    // deliver a handle's bytes in one recvmsg and its SCM_RIGHTS fd in a LATER
    // recvmsg. The pre-fix recv_native_handle_follow_up gave up the moment
    // recv_raw() returned bytes without the fd → parse_native_handle saw
    // fds.len() < num_fds → "malformed native_handle for slot 0". The fix
    // waits (bounded) for the trailing fd instead of bailing.
    #[test]
    fn recv_native_handle_waits_for_fd_in_later_recvmsg() {
        use crate::app_link::AppSession;
        use crate::blit::BlitEngine;

        let _lock = fd_guard_lock().lock().unwrap_or_else(|p| p.into_inner());
        let _guard = FdCountGuard::new("slot-fd-late");

        let (srv, cli) = socketpair();
        let mut session = AppSession::new(Transport::new(srv).unwrap());
        let mut client = Transport::new(cli).unwrap();
        let mut blit = BlitEngine::new();
        let fake_fd0 = fake_dmabuf_fd();
        let fake_fd1 = fake_dmabuf_fd();

        let handle = std::thread::spawn(move || {
            loop {
                match client.recv().unwrap() {
                    Some(Message::Hello(_)) => break,
                    None => std::thread::sleep(std::time::Duration::from_millis(5)),
                    _ => {}
                }
            }
            client
                .send(&Message::Config(proto::ConfigMessage::new(
                    100, 100, 60000, 96, 0,
                )))
                .unwrap();
            // TBUF0
            client
                .send(&Message::Slot(proto::SlotBuffer::new(
                    0, 100, 100, proto::DRM_FORMAT_ABGR8888, 400,
                )))
                .unwrap();
            // handle0 bytes WITHOUT its fd — the fd is delivered later, riding
            // on the next message's bytes (separate recvmsg).
            client.send_raw(&native_handle_bytes(), &[]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(30));

            // fd0 (for handle0) + fd1 (for the following handle1) arrive late,
            // attached to a subsequent length-prefixed TBUF1 + its handle.
            let tb1 = proto::encode(&Message::Slot(proto::SlotBuffer::new(
                1, 100, 100, proto::DRM_FORMAT_ABGR8888, 400,
            )));
            let mut wire = Vec::new();
            wire.extend_from_slice(&(tb1.len() as u32).to_le_bytes());
            wire.extend_from_slice(&tb1);
            wire.extend_from_slice(&native_handle_bytes());
            let late_fds = [fake_fd0.as_raw_fd(), fake_fd1.as_raw_fd()];
            client.send_raw(&wire, &late_fds).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        });

        loop {
            match session.do_handshake().unwrap() {
                true => break,
                false => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }

        // Slot 0 must register — the pre-fix code returns Err("malformed
        // native_handle for slot 0") here (fd-short) → RED.
        let msg0 = loop {
            match session.recv_message(&mut blit).unwrap() {
                Some(m) => break m,
                None => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        };
        assert!(matches!(msg0, Message::Slot(s) if s.slot == 0));

        // The message that carried the late fd must still decode cleanly.
        let msg1 = loop {
            match session.recv_message(&mut blit).unwrap() {
                Some(m) => break m,
                None => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        };
        assert!(matches!(msg1, Message::Slot(s) if s.slot == 1));
        assert_eq!(session.slot_count(), 2);

        handle.join().unwrap();
    }

    // A second recv() must be served from `pending` without a new recvmsg (the
    // socket is drained after the first read), preserving byte order. The
    // immediate `expect` is the observable: no EAGAIN, no poll needed.
    #[test]
    fn pending_served_without_recvmsg() {
        let (srv, cli) = socketpair();
        let mut server = Transport::new(srv).unwrap();
        let mut client = Transport::new(cli).unwrap();

        let touch = Message::Touch(proto::TouchMessage::new(
            1, 10.0, 20.0, proto::TOUCH_PHASE_DOWN, 5,
        ));
        let conf = Message::Config(proto::ConfigMessage::new(
            800, 600, 60000, 96, proto::APP_CAP_DIRECT_IMPORT,
        ));
        let key = Message::Key(proto::KeyMessage::new(10, 1, 0));

        client.send(&touch).unwrap();
        client.send(&conf).unwrap();
        client.send(&key).unwrap();

        // First call absorbs one recvmsg with all three messages.
        assert_eq!(poll_recv(&mut server), touch);
        // The remaining two are served straight from pending, in order.
        let second = server.recv().unwrap().expect("pending Config served immediately");
        assert_eq!(second, conf);
        let third = server.recv().unwrap().expect("pending Key served immediately");
        assert_eq!(third, key);
    }
}

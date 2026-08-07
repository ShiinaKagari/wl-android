use std::io;
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
        )
        .map_err(|e| {
            // Only a definitively-dead peer (EPIPE/ECONNRESET/ENOTCONN) gets
            // the teardown kick: shutdown() the socket so the next recv()
            // sees EOF and the session teardown path fires. A dead-but-Open
            // session keeps accepting SCM_RIGHTS fds into the socket buffer,
            // leaking a 32MB memfd per frame until OOM. EAGAIN etc. are
            // transient (non-blocking socket, full buffer) — the App is fine,
            // never kill the session for those.
            if matches!(
                e,
                nix::errno::Errno::EPIPE
                    | nix::errno::Errno::ECONNRESET
                    | nix::errno::Errno::ENOTCONN
            ) {
                let _ = nix::sys::socket::shutdown(
                    self.stream.as_raw_fd(),
                    nix::sys::socket::Shutdown::Both,
                );
            }
            std::io::Error::from_raw_os_error(e as i32)
        })?;
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
            // Peer closed. This is NOT an empty-read / WouldBlock case: the
            // App is gone (SIGKILL/lmkd) and the session must be torn down —
            // otherwise the caller keeps treating the dead session as Active
            // and every subsequent send_frame piles another SCM_RIGHTS fd into
            // the dead socket (per-frame memfd leak → OOM kills more apps).
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "peer closed the land socket",
            ));
        }

        self.pending.extend_from_slice(&data);
        self.pending_fds.extend(fds);

        match self.take_pending_message()? {
            Some((body, fds)) => self.decode_msg(&body, fds).map(Some),
            None => Ok(None), // partial length prefix — wait for the rest
        }
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
        // FrameMessage layout: magic@0, width@4, height@8, flags@12.
        if magic != proto::MAGIC_LAND || body.len() < 16 {
            return 0;
        }
        let flags = u32::from_le_bytes([body[12], body[13], body[14], body[15]]);
        if flags & proto::FRAME_CARRIES_FDS != 0 { 1 } else { 0 }
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

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    fn socketpair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("socketpair")
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


    /// The received fd must be a live descriptor: getsockopt succeeds and it is
    /// a stream socket (the fake dmabuf we sent is a socketpair endpoint, so a
    /// dup through SCM_RIGHTS must still be a socket).

    #[test]
    fn send_recv_config_roundtrip() {
        let (srv, cli) = socketpair();
        let mut transport = Transport::new(srv).unwrap();
        let mut reader = Transport::new(cli).unwrap();

        let conf = proto::ConfigMessage::new(800, 600, 60000, 96, 0, 0);
        transport.send(&Message::Config(conf)).unwrap();

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
        assert!(matches!(received, Message::Config(_)));
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
            800, 600, 60000, 96, 0, 0));

        client.send(&touch).unwrap();
        client.send(&conf).unwrap();

        assert_eq!(poll_recv(&mut server), touch);
        assert_eq!(poll_recv(&mut server), conf);
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
            800, 600, 60000, 96, 0, 0));
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

use std::io::{self, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use wl_android_common::proto;
use wl_android_common::proto::Message;

pub struct AppSession {
    pub write_stream: Arc<UnixStream>,
    recv_buf: Vec<u8>,
}

impl AppSession {
    /// Connect and return session. The read end is returned separately for
    /// the dedicated recv thread; the write end is shared for JNI calls.
    pub fn connect(path: &str) -> io::Result<(Self, UnixStream)> {
        let stream = UnixStream::connect(path)?;
        stream.set_nonblocking(false)?;
        let write_stream = Arc::new(stream.try_clone()?);
        let read_stream = stream;
        Ok((Self { write_stream, recv_buf: vec![0u8; 65536] }, read_stream))
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

    // ── Recv (blocking, runs on dedicated thread) ──

    pub fn run_loop(
        read_stream: UnixStream,
        write_stream: UnixStream,
        on_frame: impl Fn(u64, u32, u32, u32, &[u8]),
    ) -> io::Result<()> {
        let mut buf = vec![0u8; 65536];
        let mut rd = read_stream;
        let mut wr = write_stream;

        // 1. Receive HELO
        let (data, _fds) = Self::recv_raw_with_fds(&mut rd, &mut buf)?;
        let msg = proto::decode(&data, vec![])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        if !matches!(msg, Message::Hello(_)) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "expected HELO"));
        }

        // 2. Send CONF
        let conf_data = proto::encode(&Message::Config(proto::ConfigMessage::new(3392, 2400, 144000, 289, 0)));
        let len = (conf_data.len() as u32).to_le_bytes();
        wr.write_all(&len)?;
        wr.write_all(&conf_data)?;
        wr.flush()?;

        // 3. Frame ← Ack loop
        loop {
            let (data, fds) = match Self::recv_raw_with_fds(&mut rd, &mut buf) {
                Ok(v) => v,
                Err(e) => {
                    log::error!("recv_raw_with_fds failed: {e}");
                    return Err(e);
                }
            };
            let msg = match proto::decode(&data, fds) {
                Ok(m) => m,
                Err(e) => {
                    log::error!("proto::decode failed: {e}");
                    return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
                }
            };
            match msg {
                Message::Frame(fm, fds) => {
                    let size = fm.width as usize * fm.height as usize * 4;
                    let pixel_data = if !fds.is_empty() {
                        use std::os::fd::AsRawFd;
                        let fd = &fds[0];
                        let ptr = unsafe {
                            libc::mmap(
                                std::ptr::null_mut(),
                                size,
                                libc::PROT_READ,
                                libc::MAP_SHARED,
                                fd.as_raw_fd(),
                                0,
                            )
                        };
                        if ptr == libc::MAP_FAILED {
                            Vec::new()
                        } else {
                            let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, size) };
                            let v = slice.to_vec();
                            unsafe { libc::munmap(ptr, size); }
                            v
                        }
                    } else {
                        Vec::new()
                    };
                    on_frame(fm.serial, fm.buffer_id, fm.width, fm.height, &pixel_data);
                    let ack = proto::FrameAck::new(fm.serial);
                    let ack_data = proto::encode(&Message::Ack(ack));
                    let alen = (ack_data.len() as u32).to_le_bytes();
                    wr.write_all(&alen)?;
                    wr.write_all(&ack_data)?;
                    wr.flush()?;
                }
                Message::Config(_) | Message::Hello(_) | Message::Gone(_) => {}
                _ => {}
            }
        }
    }

    fn recv_raw_with_fds(
        stream: &mut UnixStream,
        buf: &mut Vec<u8>,
    ) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
        use std::os::fd::AsRawFd;
        use std::io::IoSliceMut;

        let mut cmsg_space = nix::cmsg_space!([std::os::fd::RawFd; 4]);
        let n_bytes;
        let fds: Vec<OwnedFd>;
        {
            let mut iov = [IoSliceMut::new(buf)];
            let msg = nix::sys::socket::recvmsg::<()>(
                stream.as_raw_fd(),
                &mut iov,
                Some(&mut cmsg_space),
                nix::sys::socket::MsgFlags::empty(),
            )?;
            n_bytes = msg.bytes;
            fds = msg
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
        }

        let data = buf[..n_bytes].to_vec();
        Ok((data, fds))
    }
}

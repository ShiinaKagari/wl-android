use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use wl_android_common::proto;
use wl_android_common::proto::Message;

pub struct AppSession {
    stream: UnixStream,
    recv_buf: Vec<u8>,
}

impl AppSession {
    pub fn connect(path: &str) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_nonblocking(false)?;
        Ok(Self { stream, recv_buf: vec![0u8; 65536] })
    }

    pub fn socket_fd(&self) -> jint_raw {
        self.stream.as_raw_fd() as jint_raw
    }

    // ── Protocol send ──

    pub fn send_message(&mut self, msg: &Message) -> io::Result<()> {
        let data = proto::encode(msg);
        let len = (data.len() as u32).to_le_bytes();
        self.stream.write_all(&len)?;
        self.stream.write_all(&data)?;
        self.stream.flush()?;
        Ok(())
    }

    pub fn send_config(&mut self, w: u32, h: u32, refresh_millihz: u32, dpi: u32) -> io::Result<()> {
        let conf = proto::ConfigMessage::new(w, h, refresh_millihz, dpi, 0);
        self.send_message(&Message::Config(conf))
    }

    // ── Protocol recv ──

    pub fn recv_message(&mut self) -> io::Result<Message> {
        let data = self.recv_raw()?;
        proto::decode(&data, vec![])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }

    fn recv_raw(&mut self) -> io::Result<Vec<u8>> {
        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf)?;
        let msg_len = u32::from_le_bytes(len_buf) as usize;

        if msg_len > self.recv_buf.len() {
            self.recv_buf.resize(msg_len, 0);
        }
        self.stream.read_exact(&mut self.recv_buf[..msg_len])?;
        Ok(self.recv_buf[..msg_len].to_vec())
    }

    // ── Full protocol cycle ──

    /// Do handshake: wait for HELO, send CONF, then enter frame+ack loop.
    /// This blocks the calling thread — intended to run on a dedicated thread.
    pub fn run_loop(&mut self, on_frame: impl Fn(u64, u32, u32, u32)) -> io::Result<()> {
        // 1. Receive HELO
        let msg = self.recv_message()?;
        if !matches!(msg, Message::Hello(_)) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "expected HELO"));
        }

        // 2. Send CONF (default values, caller will update via nativeOnConfig)
        self.send_config(3392, 2400, 144000, 289)?;

        // 3. Frame ← Ack loop
        loop {
            let msg = self.recv_message()?;
            match msg {
                Message::Frame(fm, _) => {
                    on_frame(fm.serial, fm.buffer_id, fm.width, fm.height);
                    // Send cumulative ack
                    let ack = proto::FrameAck::new(fm.serial);
                    self.send_message(&Message::Ack(ack))?;
                }
                Message::Config(conf) => {
                    // Server sends config? Ignore in v1
                    let _ = conf;
                }
                Message::Gone(gone) => {
                    // Buffer destroyed by server
                    let _ = gone.buffer_id;
                }
                Message::Hello(_) => {} // re-handshake? ignore
                _ => {}
            }
        }
    }
}

type jint_raw = std::os::raw::c_int;

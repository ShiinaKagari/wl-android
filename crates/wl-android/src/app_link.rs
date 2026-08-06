use std::io;
use std::os::unix::net::UnixListener;

use tracing::{info, warn};

use crate::transport::Transport;
use wl_android_common::proto::{self, HelloMessage, Message, PROTOCOL_VERSION};

// ── Listener ──

pub fn create_listener(path: &str) -> io::Result<UnixListener> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::remove_file(path).ok();

    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;

    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666)).ok();

    info!("land socket at {path} (non-blocking)");
    Ok(listener)
}

// ── Session ──

pub struct AppSession {
    transport: Transport,
    mode: SessionMode,
    sent_helo: bool,
    server_caps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SessionMode {
    Handshake,
    Active,
}

impl AppSession {
    pub fn new(transport: Transport) -> Self {
        Self { transport, mode: SessionMode::Handshake, sent_helo: false, server_caps: 0 }
    }

    pub fn mode(&self) -> SessionMode {
        self.mode
    }

    pub fn do_handshake(&mut self) -> io::Result<bool> {
        if !self.sent_helo {
            let mut helo = HelloMessage::default();
            // SHM/CPU frame path: advertise the SHM cap so the App skips the
            // Vulkan swapchain (CPU lock + swapchain conflict) and presents
            // pixel frames via the CPU path. The blit path was removed — the
            // only frame path is SHM memfd forwarding.
            helo.server_caps |= proto::SERVER_CAP_SHM;
            self.server_caps = helo.server_caps;
            self.transport.send(&Message::Hello(helo))?;
            info!("sent HELO (v{} caps={:#x})", PROTOCOL_VERSION, self.server_caps);
            self.sent_helo = true;
        }

        match self.transport.recv() {
            Ok(Some(Message::Config(conf))) => {
                // H-03: version check
                if conf.protocol_version != PROTOCOL_VERSION {
                    warn!(got = conf.protocol_version, expected = PROTOCOL_VERSION, "protocol version mismatch");
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "protocol version mismatch"));
                }
                info!(w = conf.width, h = conf.height, "received CONF");
                self.mode = SessionMode::Active;
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e),
            Ok(Some(other)) => {
                warn!(?other, "unexpected during handshake");
                Err(io::Error::new(io::ErrorKind::InvalidData, "expected CONF"))
            }
        }
    }

    /// P-08/P-08b: send a frame. SHM path carries the pixel fd (plane 0);
    /// no fence is ever attached (blit removed).
    #[allow(clippy::too_many_arguments)] // mirrors the wire fields 1:1
    pub fn send_frame(
        &mut self, frame_serial: u64, buffer_id: u32, _screen_w: u32, _screen_h: u32,
        buf_w: u32, buf_h: u32, pixel_fd: Option<std::os::fd::OwnedFd>,
        _fence_fd: Option<std::os::fd::OwnedFd>,
    ) -> io::Result<()> {
        let mut fm = proto::FrameMessage {
            magic: proto::MAGIC_LAND,
            num_planes: 1,
            serial: frame_serial,
            modifier: 0,
            width: buf_w,
            height: buf_h,
            drm_format: proto::DRM_FORMAT_ABGR8888,
            flags: 0,
            buffer_id,
            _reserved: 0,
            planes: [
                proto::PlaneDesc { offset: 0, stride: buf_w * 4 },
                proto::PlaneDesc { offset: 0, stride: 0 },
                proto::PlaneDesc { offset: 0, stride: 0 },
                proto::PlaneDesc { offset: 0, stride: 0 },
            ],
        };
        let mut fds: Vec<std::os::fd::OwnedFd> = pixel_fd.into_iter().collect();
        fm.set_carries_fds(!fds.is_empty());
        if let Some(fence) = _fence_fd {
            fm.set_carries_fence(true);
            fds.push(fence);
        }
        self.transport.send(&Message::Frame(fm, fds))
    }

    #[allow(dead_code)]
    pub fn try_recv_ack(&mut self) -> io::Result<Option<u64>> {
        match self.transport.recv() {
            Ok(Some(Message::Ack(ack))) => Ok(Some(ack.serial)),
            Ok(None) => Ok(None), // EAGAIN
            Ok(Some(Message::Config(_))) => Ok(None),
            Ok(Some(Message::Touch(_))) => Ok(None), // handled by caller, not an ack
            Ok(Some(other)) => {
                warn!(?other, "unexpected message");
                Ok(None)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Server → App display-geometry/DPI/refresh update. Sent when the server
    /// buckets the DPI (e.g. 289 → 288) or applies a size/refresh change, so
    /// the App resizes its render window / HiDPI scale to match.
    pub fn send_config_update(&mut self, w: u32, h: u32, refresh_millihz: u32, dpi: u32, frame_mode: u32) -> io::Result<()> {
        let conf = proto::ConfigMessage::new(w, h, refresh_millihz, dpi, 0, frame_mode)
            .as_config_update();
        self.transport.send(&Message::ConfigUpdate(conf))
    }

    /// Receive any message from the App (non-blocking). Pure passthrough of
    /// the transport decode — the blit/TBUF/slot path was removed, so the
    /// only messages expected here are Ack/Touch/Key/Config.
    pub fn recv_message(&mut self) -> io::Result<Option<Message>> {
        self.transport.recv()
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;

    // X-04: fd leak guard (process-global, serialized across tests).
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
            let initial = std::fs::read_dir("/proc/self/fd").map(|e| e.count()).unwrap_or(0);
            Self { initial, label }
        }
    }
    impl Drop for FdCountGuard {
        fn drop(&mut self) {
            let current = std::fs::read_dir("/proc/self/fd").map(|e| e.count()).unwrap_or(0);
            assert_eq!(current, self.initial, "fd leak [{}]: {} != {}", self.label, current, self.initial);
        }
    }

    fn socketpair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("socketpair")
    }

    /// A real, readable fd for pixel frames (a memfd works for the wire test).
    fn fake_pixel_fd() -> OwnedFd {
        let m = nix::sys::memfd::memfd_create("test-pixel", nix::sys::memfd::MFdFlags::MFD_CLOEXEC).unwrap();
        m
    }

    #[test]
    fn handshake_completes_with_valid_conf() {
        let _lock = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("handshake_completes_with_valid_conf");
        let (client, server) = socketpair();
        let mut session = AppSession::new(Transport::new(server).unwrap());

        std::thread::spawn(move || {
            let mut client = Transport::new(client).unwrap();
            // wait for HELO (0 fds — raw bytes suffice)
            for _ in 0..100 {
                if client.recv().unwrap().is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let conf = proto::ConfigMessage::new(800, 600, 60000, 96, 0, 0);
            client.send(&Message::Config(conf)).unwrap();
        });

        // do_handshake: first call sends HELO (returns Ok(false) waiting CONF),
        // subsequent calls read CONF.
        let _ = session.do_handshake(); // sends HELO
        let mut done = false;
        for _ in 0..50 {
            if session.do_handshake().unwrap_or(false) {
                done = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(done, "handshake did not complete");
        assert_eq!(session.mode(), SessionMode::Active);
        assert!(session.server_caps & proto::SERVER_CAP_SHM != 0);
    }

    #[test]
    fn frame_ack_roundtrip() {
        let _lock = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("frame_ack_roundtrip");
        let (client, server) = socketpair();
        let mut session = AppSession::new(Transport::new(server).unwrap());

        let client_thread = std::thread::spawn(move || {
            let mut client = Transport::new(client).unwrap();
            // wait for the frame message, then ack it
            loop {
                match client.recv().unwrap() {
                    Some(Message::Frame(..)) => break,
                    Some(_) => panic!("expected Frame"),
                    None => std::thread::sleep(std::time::Duration::from_millis(10)),
                }
            }
            client.send(&Message::Ack(proto::FrameAck::new(42))).unwrap();
        });

        // send a frame
        let fd = fake_pixel_fd();
        session.send_frame(42, 1, 800, 600, 800, 600, Some(fd), None).unwrap();

        // server reads the ack
        let mut got = None;
        for _ in 0..50 {
            if let Ok(Some(serial)) = session.try_recv_ack() {
                got = Some(serial);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        client_thread.join().unwrap();
        assert_eq!(got, Some(42));
    }

    #[test]
    fn send_frame_shm_carries_pixel_fd_no_fence() {
        let _lock = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("send_frame_shm_carries_pixel_fd_no_fence");
        let (client, server) = socketpair();
        let mut session = AppSession::new(Transport::new(server).unwrap());

        let client_thread = std::thread::spawn(move || {
            let mut client = Transport::new(client).unwrap();
            loop {
                match client.recv().unwrap() {
                    Some(msg) => return msg,
                    None => std::thread::sleep(std::time::Duration::from_millis(10)),
                }
            }
        });

        let fd = fake_pixel_fd();
        let raw = fd.as_raw_fd();
        session.send_frame(7, 2, 800, 600, 800, 600, Some(fd), None).unwrap();
        let msg = client_thread.join().unwrap();
        match msg {
            Message::Frame(fm, fds) => {
                assert!(fm.carries_fds(), "SHM frame must carry the pixel fd");
                assert!(!fm.carries_fence(), "SHM frame must NOT carry a fence");
                assert_eq!(fds.len(), 1);
                assert_eq!(fds[0].as_raw_fd(), raw);
            }
            _ => panic!("expected Frame"),
        }
    }
}

use std::io;
use std::os::unix::net::UnixListener;

use tracing::info;

use crate::transport::Transport;
use wl_android_common::proto::{self, Message};

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
//
// Stateless session: no handshake, no serial, no ack. The server pushes the
// current frame whenever KWin commits (the App replies with RELEASE to hand
// the memfd back); the App sends CONF/TOUC/KEYM as plain events. Every
// message is independently processable — there is no ordering contract.

pub struct AppSession {
    transport: Transport,
}

impl AppSession {
    pub fn new(transport: Transport) -> Self {
        Self { transport }
    }

    /// Send the CURRENT frame (width/height + pixel fd). The App replies
    /// with a RELEASE once it has consumed the fd.
    pub fn send_frame(
        &mut self,
        buf_w: u32,
        buf_h: u32,
        pixel_fd: std::os::fd::OwnedFd,
    ) -> io::Result<()> {
        let fm = proto::FrameMessage::new(buf_w, buf_h);
        self.transport.send(&Message::Frame(fm, vec![pixel_fd]))
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
    /// the transport decode — the stateless protocol only carries
    /// Config/Touch/Key/Release.
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
    use wl_android_common::proto::PROTOCOL_VERSION;

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
    fn send_frame_carries_pixel_fd_and_dimensions() {
        let _lock = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("send_frame_carries_pixel_fd_and_dimensions");
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
        session.send_frame(800, 600, fd).unwrap();
        let msg = client_thread.join().unwrap();
        match msg {
            Message::Frame(fm, fds) => {
                assert!(fm.carries_fds(), "frame must carry the pixel fd");
                assert_eq!((fm.width, fm.height), (800, 600));
                assert_eq!(fds.len(), 1);
                assert_eq!(fds[0].as_raw_fd(), raw);
            }
            _ => panic!("expected Frame"),
        }
    }

    #[test]
    fn release_roundtrip() {
        let _lock = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("release_roundtrip");
        let (client, server) = socketpair();
        let mut session = AppSession::new(Transport::new(server).unwrap());

        let client_thread = std::thread::spawn(move || {
            let mut client = Transport::new(client).unwrap();
            client.send(&Message::Release(proto::ReleaseMessage::new())).unwrap();
        });

        let mut got = None;
        for _ in 0..50 {
            if let Ok(Some(msg)) = session.recv_message() {
                got = Some(msg);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        client_thread.join().unwrap();
        assert!(matches!(got, Some(Message::Release(_))), "server must receive the Release");
    }

    #[test]
    fn config_update_roundtrip() {
        let _lock = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("config_update_roundtrip");
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

        session.send_config_update(800, 600, 60000, 96, 0).unwrap();
        let msg = client_thread.join().unwrap();
        match msg {
            Message::ConfigUpdate(c) => {
                assert!(c.is_config_update());
                assert_eq!((c.width, c.height), (800, 600));
                assert_eq!(c.protocol_version, PROTOCOL_VERSION);
            }
            _ => panic!("expected ConfigUpdate"),
        }
    }

    #[test]
    fn config_version_check_uses_protocol_version() {
        // The stateless protocol drops the handshake; CONF events still
        // carry the version so a mismatched App is detectable.
        let conf = proto::ConfigMessage::new(800, 600, 60000, 96, 0, 0);
        assert_eq!(conf.protocol_version, PROTOCOL_VERSION);
    }
}

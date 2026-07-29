use std::io;
use std::os::unix::net::UnixListener;

use tracing::{debug, info, warn};

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
    slot_count: u32,
    server_caps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SessionMode {
    Handshake,
    /// Waiting for TBUF slot registrations (blit mode only, H-04)
    SlotRegistration,
    Active,
}

impl AppSession {
    pub fn new(transport: Transport) -> Self {
        Self { transport, mode: SessionMode::Handshake, sent_helo: false, slot_count: 0, server_caps: 0 }
    }

    pub fn mode(&self) -> SessionMode {
        self.mode
    }

    pub fn do_handshake(&mut self) -> io::Result<bool> {
        if !self.sent_helo {
            let helo = HelloMessage::default();
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

                // H-04: mode selection
                if conf.app_caps & proto::APP_CAP_DIRECT_IMPORT != 0 {
                    info!("mode: direct (App supports dma_buf import)");
                    self.mode = SessionMode::Active;
                } else if self.server_caps & proto::SERVER_CAP_BLIT != 0 {
                    info!("mode: blit (waiting for slot registration)");
                    self.mode = SessionMode::SlotRegistration;
                } else {
                    warn!("no available frame path");
                    return Err(io::Error::other("no available frame path"));
                }
                Ok(true)
            }
            Ok(None) => Ok(false),
            Ok(Some(other)) => {
                warn!(?other, "unexpected during handshake");
                Err(io::Error::new(io::ErrorKind::InvalidData, "expected CONF"))
            }
            Err(e) => Err(e),
        }
    }

    pub fn send_frame(
        &mut self, frame_serial: u64, buffer_id: u32, width: u32, height: u32,
    ) -> io::Result<()> {
        let mut fm = proto::FrameMessage {
            magic: proto::MAGIC_LAND,
            num_planes: 1,
            serial: frame_serial,
            modifier: 0,
            width,
            height,
            drm_format: proto::DRM_FORMAT_ABGR8888,
            flags: 0,
            buffer_id,
            _reserved: 0,
            planes: [
                proto::PlaneDesc { offset: 0, stride: width * 4 },
                proto::PlaneDesc { offset: 0, stride: 0 },
                proto::PlaneDesc { offset: 0, stride: 0 },
                proto::PlaneDesc { offset: 0, stride: 0 },
            ],
        };
        fm.set_carries_fds(false);
        self.transport.send(&Message::Frame(fm, vec![]))
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

    /// Receive any message from the App (non-blocking).
    /// For SlotBuffer messages, also receives the following native_handle (P-13).
    pub fn recv_message(&mut self) -> io::Result<Option<Message>> {
        match self.transport.recv() {
            Ok(Some(Message::Slot(slot))) => {
                self.slot_count += 1;
                info!(slot = slot.slot, count = self.slot_count, "slot registered");
                // Recv the native_handle that follows (P-13)
                if let Ok(Some((_data, fds))) = self.transport.recv_raw() {
                    if let Some(handle) = crate::ahb_handle::parse_native_handle(&_data, fds) {
                        debug!(slot = slot.slot, num_fds = handle.num_fds, "native_handle parsed");
                        // TODO M6b: import fds into Vulkan for blit
                        let _ = handle.fds;
                    } else {
                        warn!(slot = slot.slot, "failed to parse native_handle");
                    }
                }
                Ok(Some(Message::Slot(slot)))
            }
            Ok(Some(msg)) => Ok(Some(msg)),
            Ok(None) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn slot_count(&self) -> u32 {
        self.slot_count
    }

    pub fn activate(&mut self) {
        self.mode = SessionMode::Active;
    }

    pub fn send_gone(&mut self, buffer_id: u32) -> io::Result<()> {
        let gone = proto::BufferGone::new(buffer_id);
        self.transport.send(&Message::Gone(gone))
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

    // H-01, H-02: HELO→CONF handshake (non-blocking, requires multiple polls)
    #[test]
    fn handshake_completes_with_valid_conf() {
        let (srv, cli) = socketpair();
        let mut session = AppSession::new(Transport::new(srv).unwrap());
        let mut client = Transport::new(cli).unwrap();

        let handle = std::thread::spawn(move || {
            // Client: wait for HELO, respond with CONF
            loop {
                match client.recv().unwrap() {
                    Some(Message::Hello(_)) => {
                        client
                            .send(&Message::Config(proto::ConfigMessage::new(
                                800, 600, 60000, 96, proto::APP_CAP_DIRECT_IMPORT,
                            )))
                            .unwrap();
                        break;
                    }
                    None => std::thread::sleep(std::time::Duration::from_millis(10)),
                    _ => {}
                }
            }
        });

        // Non-blocking poll: call do_handshake repeatedly until complete
        let mut attempts = 0;
        loop {
            match session.do_handshake().unwrap() {
                true => break,
                false => {
                    attempts += 1;
                    if attempts > 100 {
                        panic!("handshake timed out");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }

        handle.join().unwrap();
    }

    // P-11, F-11: Frame → Ack
    #[test]
    fn frame_ack_roundtrip() {
        let (srv, cli) = socketpair();
        let mut session = AppSession::new(Transport::new(srv).unwrap());
        let mut client = Transport::new(cli).unwrap();

        let handle = std::thread::spawn(move || {
            // Wait for HELO
            loop {
                match client.recv().unwrap() {
                    Some(Message::Hello(_)) => break,
                    None => std::thread::sleep(std::time::Duration::from_millis(5)),
                    _ => {}
                }
            }
            // Send CONF
            client
                .send(&Message::Config(proto::ConfigMessage::new(
                    100, 100, 60000, 96, 0,
                )))
                .unwrap();

            // Wait for Frame
            loop {
                match client.recv().unwrap() {
                    Some(Message::Frame(fm, _)) => {
                        assert_eq!(fm.serial, 7);
                        assert_eq!(fm.width, 100);
                        assert_eq!(fm.height, 100);
                        // Ack
                        client
                            .send(&Message::Ack(proto::FrameAck::new(7)))
                            .unwrap();
                        break;
                    }
                    None => std::thread::sleep(std::time::Duration::from_millis(5)),
                    other => panic!("expected Frame, got {other:?}"),
                }
            }
        });

        // Server: handshake
        loop {
            match session.do_handshake().unwrap() {
                true => break,
                false => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }

        // Send frame
        session.send_frame(7, 1, 100, 100).unwrap();

        // Receive ack
        let ack = loop {
            match session.try_recv_ack().unwrap() {
                Some(s) => break s,
                None => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        };
        assert_eq!(ack, 7);

        handle.join().unwrap();
    }
}

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixListener;

use tracing::{debug, info, warn};

use crate::blit::BlitEngine;
use crate::transport::Transport;
use wl_android_common::proto::{self, HelloMessage, Message, PROTOCOL_VERSION};

/// How long recv_native_handle_follow_up waits for a native_handle's trailing
/// SCM_RIGHTS fds (which can arrive in a later recvmsg than the bytes on
/// SOCK_STREAM) before giving up. Mirrors the pre-fix poll budget (~100 ms).
const HANDLE_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

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
                    info!("mode: direct");
                    self.mode = SessionMode::Active;
                } else if self.server_caps & proto::SERVER_CAP_BLIT != 0 {
                    // H-04 v2: blit waits for SLOT_COUNT TBUFs before sending frames.
                    info!("mode: blit (waiting for {} TBUF registrations)", proto::SLOT_COUNT);
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

    /// P-08/P-08b: plane fds first (FRAME_CARRIES_FDS), then at most one
    /// fence fd (FRAME_CARRIES_FENCE). Blit frames pass `pixel_fd: None` —
    /// the App owns the slot buffer and only needs the sync_file fence (F-08).
    #[allow(clippy::too_many_arguments)] // mirrors the wire fields 1:1
    pub fn send_frame(
        &mut self, frame_serial: u64, buffer_id: u32, _screen_w: u32, _screen_h: u32,
        buf_w: u32, buf_h: u32, pixel_fd: Option<std::os::fd::OwnedFd>,
        fence_fd: Option<std::os::fd::OwnedFd>,
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
        if let Some(fence) = fence_fd {
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

    /// Receive any message from the App (non-blocking).
    /// For SlotBuffer messages, also receives the following native_handle (P-13)
    /// and imports the dmabuf fd into `blit` (M6b).
    pub fn recv_message(&mut self, blit: &mut BlitEngine) -> io::Result<Option<Message>> {
        match self.transport.recv() {
            Ok(Some(Message::Slot(slot))) => {
                self.slot_count += 1;
                info!(slot = slot.slot, count = self.slot_count, "slot registered");
                // P-13: each TBUF is immediately followed by exactly one native_handle
                // message (raw wire format + SCM_RIGHTS fds).
                if let Some((data, fds)) = self.recv_native_handle_follow_up() {
                    match crate::ahb_handle::parse_native_handle(&data, fds) {
                        Some(handle) => {
                            debug!(slot = slot.slot, num_fds = handle.num_fds, "native_handle parsed");
                            if let Some(fd) = handle.fds.into_iter().next() {
                                match blit.register_slot(
                                    slot.slot,
                                    fd,
                                    slot.width,
                                    slot.height,
                                    // DRM_FORMAT_ABGR8888 is R,G,B,A memory order → VK R8G8B8A8 (DESIGN §9.2)
                                    ash::vk::Format::R8G8B8A8_UNORM,
                                    // v1 TBUF carries DRM_FORMAT_MOD_LINEAR
                                    slot.modifier,
                                ) {
                                    Ok(handle) => {
                                        info!(slot = slot.slot, handle, "slot image imported into Vulkan")
                                    }
                                    Err(e) => warn!(
                                        slot = slot.slot, err = %e,
                                        "slot import failed (server-side); slot unavailable",
                                    ),
                                }
                            } else {
                                warn!(slot = slot.slot, "native_handle carries no fds");
                            }
                        }
                        None => {
                            // P-13: 解析失败 → 明确报错并断开
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("malformed native_handle for slot {}", slot.slot),
                            ));
                        }
                    }
                } else {
                    // P-13: the handle MUST follow the TBUF; a missing one desyncs the stream.
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("missing native_handle after TBUF slot {}", slot.slot),
                    ));
                }
                Ok(Some(Message::Slot(slot)))
            }
            Ok(Some(msg)) => Ok(Some(msg)),
            Ok(None) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// P-13 follow-up: the native_handle raw message immediately follows each
    /// TBUF. Consumes exactly ONE handle message.
    ///
    /// The wire format is `GraphicBuffer::flatten()` output (32-byte `'GBFR'`
    /// header, fds via SCM_RIGHTS) — NOT the libcutils native_handle layout.
    /// AOSP's `AHardwareBuffer_sendHandleToUnixSocket` sends the flattened
    /// bytes and the fds as SCM_RIGHTS ancillary data. The flatten header has
    /// NO fd count — the fd count is whatever the cmsg delivered.
    ///
    /// On SOCK_STREAM the kernel can deliver a handle's bytes in one recvmsg
    /// and its SCM_RIGHTS fds in a later one, so an fd-short first chunk must
    /// wait instead of bailing. Any bytes/fds that followed the handle inside
    /// the same coalesced recvmsg (the slot-registration burst) are pushed
    /// back into the transport so the next message decodes intact instead of
    /// being silently dropped.
    fn recv_native_handle_follow_up(&mut self) -> Option<(Vec<u8>, Vec<OwnedFd>)> {
        let deadline = std::time::Instant::now() + HANDLE_WAIT_TIMEOUT;

        // First raw chunk — serves pending (the handle coalesced right after the
        // TBUF) or one recvmsg. Bounded poll absorbs socket-scheduling gaps.
        let (mut data, mut fds) = loop {
            match self.transport.recv_raw() {
                Ok(Some((d, f))) => break (d, f),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(_) => return None,
            }
        };

        // Decide the format from the magic once we have >= 4 bytes.
        let is_flat = {
            let four = &data[..data.len().min(4)];
            four.len() == 4 && u32::from_le_bytes([four[0], four[1], four[2], four[3]])
                == crate::ahb_handle::FLAT_MAGIC
        };
        if data.len() < 32 {
            tracing::debug!(
                n = data.len(),
                fds = fds.len(),
                first = data.iter().take(8).map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "),
                "handle follow-up: short first chunk"
            );
        }

        if is_flat {
            // GraphicBuffer::flatten: 32-byte fixed header + SCM_RIGHTS fds.
            // The header has no fd count — one image carries exactly one
            // dma-buf fd (P-12). Wait for the full header + at least one fd,
            // then take exactly one fd and preserve any surplus bytes/fds.
            loop {
                if data.len() >= 32 && !fds.is_empty() {
                    let handle_data = data[..32].to_vec();
                    let mut fds = fds;
                    let handle_fds: Vec<OwnedFd> = fds.drain(..1).collect();
                    let leftover_data = data[32..].to_vec();
                    let leftover_fds = fds;
                    if !leftover_data.is_empty() || !leftover_fds.is_empty() {
                        self.transport.unrecv_raw(leftover_data, leftover_fds);
                    }
                    return Some((handle_data, handle_fds));
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let want_fds = if fds.is_empty() { 1 } else { 0 };
                match self.transport.recv_raw_with_fd_wait(want_fds, remaining) {
                    Ok(Some((d, f))) => {
                        data.extend_from_slice(&d);
                        fds.extend(f);
                    }
                    Ok(None) => return Some((data, fds)), // timeout — parse reports it
                    Err(_) => return None,
                }
            }
        }

        // Legacy libcutils native_handle layout: header-driven byte/fd counts.
        loop {
            if data.len() < 12 {
                // Split header — accumulate more bytes (and any fds riding along).
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                match self.transport.recv_raw_with_fd_wait(0, remaining) {
                    Ok(Some((d, f))) => {
                        data.extend_from_slice(&d);
                        fds.extend(f);
                    }
                    Ok(None) => return Some((data, fds)), // timeout — parse reports it
                    Err(_) => return None,
                }
                continue;
            }
            let num_fds = i32::from_le_bytes([data[4], data[5], data[6], data[7]]);
            let num_ints = i32::from_le_bytes([data[8], data[9], data[10], data[11]]);
            if num_fds < 0 || num_ints < 0 {
                return None; // malformed header
            }
            let (num_fds, num_ints) = (num_fds as usize, num_ints as usize);
            let expected_len = 12 + num_ints * 4;

            if fds.len() < num_fds {
                // fd-short: the SCM_RIGHTS fds trail the bytes on SOCK_STREAM.
                // Wait (bounded) instead of failing — this was the device's
                // "malformed native_handle for slot 0".
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                match self.transport.recv_raw_with_fd_wait(num_fds, remaining) {
                    Ok(Some((d, f))) => {
                        data.extend_from_slice(&d);
                        fds.extend(f);
                    }
                    Ok(None) => return Some((data, fds)), // timeout — parse fails fd-short
                    Err(_) => return None,
                }
                continue;
            }
            if data.len() < expected_len {
                // Bytes short — accumulate the rest of the handle.
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                match self.transport.recv_raw_with_fd_wait(0, remaining) {
                    Ok(Some((d, f))) => {
                        data.extend_from_slice(&d);
                        fds.extend(f);
                    }
                    Ok(None) => return Some((data, fds)), // timeout — parse fails short
                    Err(_) => return None,
                }
                continue;
            }

            // Complete handle: take exactly its bytes + fds and preserve any
            // bytes/fds that followed it in the same recvmsg (coalesced burst).
            let handle_data = data[..expected_len].to_vec();
            let handle_fds: Vec<OwnedFd> = fds.drain(..num_fds).collect();
            let leftover_data = data[expected_len..].to_vec();
            let leftover_fds = fds;
            if !leftover_data.is_empty() || !leftover_fds.is_empty() {
                self.transport.unrecv_raw(leftover_data, leftover_fds);
            }
            return Some((handle_data, handle_fds));
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
    use std::os::fd::{AsRawFd, OwnedFd, RawFd};
    use std::os::unix::net::UnixStream;

    // X-04: fd leak guard. Mirrors frame_cache::tests::FdCountGuard — /proc/self/fd
    // is process-global, so guard tests are only deterministic while no other
    // fd-touching test runs concurrently. Combine with `--test-threads=1`.
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

    // native_handle wire format with numFds=1, numInts=0 (P-13)
    fn native_handle_bytes() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&(-1i32).to_le_bytes()); // version placeholder
        data.extend_from_slice(&1i32.to_le_bytes());    // numFds
        data.extend_from_slice(&0i32.to_le_bytes());    // numInts
        data
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

    // P-11, F-11, H-04: blit mode — Frame only after SLOT_COUNT TBUF registrations.
    // app_caps=0 → blit → SlotRegistration; the client registers 3 TBUFs (each
    // followed by its native_handle, P-13) before the server activates and sends.
    #[test]
    fn frame_ack_roundtrip() {
        let _lock = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("frame-ack-rt");

        let (srv, cli) = socketpair();
        let mut session = AppSession::new(Transport::new(srv).unwrap());
        let mut client = Transport::new(cli).unwrap();
        let mut blit = BlitEngine::new(); // uninitialized → import errors, flow continues

        // One fake dmabuf fd per TBUF slot (P-13 SCM_RIGHTS payload).
        let fake_fds: Vec<OwnedFd> = (0..proto::SLOT_COUNT).map(|_| fake_dmabuf_fd()).collect();
        let raw_fds: Vec<RawFd> = fake_fds.iter().map(|fd| fd.as_raw_fd()).collect();

        let handle = std::thread::spawn(move || {
            // Wait for HELO
            loop {
                match client.recv().unwrap() {
                    Some(Message::Hello(_)) => break,
                    None => std::thread::sleep(std::time::Duration::from_millis(5)),
                    _ => {}
                }
            }
            // Send CONF with app_caps=0 → blit mode (H-04)
            client
                .send(&Message::Config(proto::ConfigMessage::new(
                    100, 100, 60000, 96, 0,
                )))
                .unwrap();

            // H-04: blit waits for SLOT_COUNT TBUFs; each TBUF is followed by the
            // native_handle wire message carrying the dmabuf fd (P-13).
            for (slot, &raw_fd) in raw_fds.iter().enumerate() {
                std::thread::sleep(std::time::Duration::from_millis(20));
                client
                    .send(&Message::Slot(proto::SlotBuffer::new(
                        slot as u32, 100, 100, proto::DRM_FORMAT_ABGR8888, 400,
                    )))
                    .unwrap();
                std::thread::sleep(std::time::Duration::from_millis(20));
                client.send_raw(&native_handle_bytes(), &[raw_fd]).unwrap();
            }

            // Wait for Frame
            let mut attempts = 0;
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
                    None => {
                        attempts += 1;
                        if attempts > 400 {
                            panic!("expected Frame, timed out (never activated)");
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    other => panic!("expected Frame, got {other:?}"),
                }
            }
        });

        // Server: handshake → blit waits for slot registrations (H-04)
        loop {
            match session.do_handshake().unwrap() {
                true => break,
                false => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
        assert_eq!(session.mode(), SessionMode::SlotRegistration);

        // Register all slots (mirrors main.rs SlotRegistration branch)
        for _ in 0..proto::SLOT_COUNT {
            let msg = loop {
                match session.recv_message(&mut blit).unwrap() {
                    Some(m) => break m,
                    None => std::thread::sleep(std::time::Duration::from_millis(5)),
                }
            };
            assert!(matches!(msg, Message::Slot(_)));
        }
        assert_eq!(session.slot_count(), proto::SLOT_COUNT as u32);
        session.activate();
        assert_eq!(session.mode(), SessionMode::Active);

        // Send frame
        session.send_frame(7, 1, 100, 100, 100, 100, None, None).unwrap();

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

    // P-13 + X-04: a Slot message with a parseable native_handle reaches the
    // blit engine. With an uninitialized engine import fails, but the session
    // must not panic and the received fds must be closed — never silently leaked.
    #[test]
    fn slot_fd_imported_not_dropped() {
        let _lock = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("slot-registration-fd-leak");

        let (srv, cli) = socketpair();
        let mut session = AppSession::new(Transport::new(srv).unwrap());
        let mut client = Transport::new(cli).unwrap();
        let mut blit = BlitEngine::new();

        let fake_fds: Vec<OwnedFd> = (0..proto::SLOT_COUNT).map(|_| fake_dmabuf_fd()).collect();
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
            for (slot, &raw_fd) in raw_fds.iter().enumerate() {
                std::thread::sleep(std::time::Duration::from_millis(20));
                client
                    .send(&Message::Slot(proto::SlotBuffer::new(
                        slot as u32, 100, 100, proto::DRM_FORMAT_ABGR8888, 400,
                    )))
                    .unwrap();
                std::thread::sleep(std::time::Duration::from_millis(20));
                client.send_raw(&native_handle_bytes(), &[raw_fd]).unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        });

        loop {
            match session.do_handshake().unwrap() {
                true => break,
                false => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
        assert_eq!(session.mode(), SessionMode::SlotRegistration);

        for _ in 0..proto::SLOT_COUNT {
            let msg = loop {
                match session.recv_message(&mut blit).unwrap() {
                    Some(m) => break m,
                    None => std::thread::sleep(std::time::Duration::from_millis(5)),
                }
            };
            assert!(matches!(msg, Message::Slot(_)));
        }
        assert_eq!(session.slot_count(), proto::SLOT_COUNT as u32);

        // Engine uninitialized → import_dmabuf errors → registry stays empty.
        handle.join().unwrap();
        assert!(blit.slots.is_empty());
    }

    // P-13: a malformed native_handle after a TBUF is an explicit error the
    // caller (main.rs) turns into a disconnect — not a silent drop.
    #[test]
    fn malformed_native_handle_disconnects() {
        let _lock = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("malformed-native-handle");

        let (srv, cli) = socketpair();
        let mut session = AppSession::new(Transport::new(srv).unwrap());
        let mut client = Transport::new(cli).unwrap();
        let mut blit = BlitEngine::new();
        let fake_fd = fake_dmabuf_fd();

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
            std::thread::sleep(std::time::Duration::from_millis(20));
            client
                .send(&Message::Slot(proto::SlotBuffer::new(
                    0, 100, 100, proto::DRM_FORMAT_ABGR8888, 400,
                )))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            // native_handle claims numFds=2 but only 1 fd is sent → parse failure
            let mut data = Vec::new();
            data.extend_from_slice(&(-1i32).to_le_bytes());
            data.extend_from_slice(&2i32.to_le_bytes());
            data.extend_from_slice(&0i32.to_le_bytes());
            client.send_raw(&data, &[fake_fd.as_raw_fd()]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        });

        loop {
            match session.do_handshake().unwrap() {
                true => break,
                false => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }

        let err = loop {
            match session.recv_message(&mut blit) {
                Ok(Some(_)) => panic!("expected parse error, got a message"),
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
                Err(e) => break e,
            }
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

        handle.join().unwrap();
    }

    // P-08b / TODO 23: a frame sent with both a pixel fd and a fence fd must
    // set FRAME_CARRIES_FDS + FRAME_CARRIES_FENCE and carry num_planes + 1 fds
    // (plane fds first, fence fd last). Roundtrip-decoded by the real Transport.
    #[test]
    fn send_frame_with_fence_appends_fence_fd_and_sets_flag() {
        let _lock = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("frame-pixel-and-fence");

        let (srv, cli) = socketpair();
        let mut session = AppSession::new(Transport::new(srv).unwrap());
        let mut client = Transport::new(cli).unwrap();

        let pixel_end = fake_dmabuf_fd();
        let fence_end = fake_dmabuf_fd();
        session
            .send_frame(9, 2, 100, 100, 100, 100, Some(pixel_end), Some(fence_end))
            .unwrap();

        let msg = loop {
            match client.recv().unwrap() {
                Some(m) => break m,
                None => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        };
        match msg {
            Message::Frame(fm, fds) => {
                assert_eq!(fm.serial, 9);
                assert_eq!(fm.buffer_id, 2);
                assert!(fm.carries_fds(), "pixel fd present → FRAME_CARRIES_FDS");
                assert!(fm.carries_fence(), "fence fd present → FRAME_CARRIES_FENCE");
                assert_eq!(proto::fd_count(&Message::Frame(fm, vec![])), 2);
                assert_eq!(fds.len(), 2, "num_planes (1) + fence (1) fds, P-08b");
            }
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    // F-08 blit mode: the App owns the slot buffer, so a blitted frame carries
    // NO pixel fd — only the sync_file fence fd (flags = fence-only LAND).
    #[test]
    fn send_frame_fence_only_carries_single_fd() {
        let _lock = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("frame-fence-only");

        let (srv, cli) = socketpair();
        let mut session = AppSession::new(Transport::new(srv).unwrap());
        let mut client = Transport::new(cli).unwrap();

        let fence_end = fake_dmabuf_fd();
        session
            .send_frame(10, 3, 100, 100, 100, 100, None, Some(fence_end))
            .unwrap();

        let msg = loop {
            match client.recv().unwrap() {
                Some(m) => break m,
                None => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        };
        match msg {
            Message::Frame(fm, fds) => {
                assert_eq!(fm.serial, 10);
                assert_eq!(fm.buffer_id, 3);
                assert!(!fm.carries_fds(), "blit frame carries no pixel fd (F-08)");
                assert!(fm.carries_fence());
                assert_eq!(proto::fd_count(&Message::Frame(fm, vec![])), 1);
                assert_eq!(fds.len(), 1, "exactly the fence fd");
            }
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    // X-06: full blit-mode session flow over a real socketpair with the real
    // AppSession: handshake (app_caps=0 → blit) → SLOT_COUNT TBUFs + native
    // handles → activate → a fence-only frame reaches the client with
    // FRAME_CARRIES_FENCE set and exactly one fd → cumulative ack returns.
    // (Slot import itself fails on host — no real dmabufs — which is fine: the
    // session flow under test is transport-level.)
    #[test]
    fn blit_mode_frame_with_fence_roundtrip() {
        let _lock = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("blit-fence-roundtrip");

        let (srv, cli) = socketpair();
        let mut session = AppSession::new(Transport::new(srv).unwrap());
        let mut client = Transport::new(cli).unwrap();
        let mut blit = BlitEngine::new(); // uninitialized → slot import errors, flow continues

        let fake_fds: Vec<OwnedFd> = (0..proto::SLOT_COUNT).map(|_| fake_dmabuf_fd()).collect();
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
            for (slot, &raw_fd) in raw_fds.iter().enumerate() {
                std::thread::sleep(std::time::Duration::from_millis(20));
                client
                    .send(&Message::Slot(proto::SlotBuffer::new(
                        slot as u32, 100, 100, proto::DRM_FORMAT_ABGR8888, 400,
                    )))
                    .unwrap();
                std::thread::sleep(std::time::Duration::from_millis(20));
                client.send_raw(&native_handle_bytes(), &[raw_fd]).unwrap();
            }

            // Expect the fence-only blit frame
            let mut attempts = 0;
            loop {
                match client.recv().unwrap() {
                    Some(Message::Frame(fm, fds)) => {
                        assert_eq!(fm.serial, 42);
                        assert_eq!(fm.buffer_id, 1, "blit frame buffer_id is the slot id");
                        assert!(!fm.carries_fds(), "no pixel fd in blit mode (F-08)");
                        assert!(fm.carries_fence(), "FRAME_CARRIES_FENCE expected");
                        assert_eq!(fds.len(), 1);
                        client
                            .send(&Message::Ack(proto::FrameAck::new(42)))
                            .unwrap();
                        break;
                    }
                    None => {
                        attempts += 1;
                        if attempts > 400 {
                            panic!("expected fence-carrying Frame, timed out");
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    other => panic!("expected Frame, got {other:?}"),
                }
            }
        });

        loop {
            match session.do_handshake().unwrap() {
                true => break,
                false => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
        assert_eq!(session.mode(), SessionMode::SlotRegistration);

        for _ in 0..proto::SLOT_COUNT {
            let msg = loop {
                match session.recv_message(&mut blit).unwrap() {
                    Some(m) => break m,
                    None => std::thread::sleep(std::time::Duration::from_millis(5)),
                }
            };
            assert!(matches!(msg, Message::Slot(_)));
        }
        session.activate();

        // Fence-only blit frame for slot 1
        let fence_end = fake_dmabuf_fd();
        session
            .send_frame(42, 1, 100, 100, 100, 100, None, Some(fence_end))
            .unwrap();

        let ack = loop {
            match session.try_recv_ack().unwrap() {
                Some(s) => break s,
                None => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        };
        assert_eq!(ack, 42);

        handle.join().unwrap();
    }
}

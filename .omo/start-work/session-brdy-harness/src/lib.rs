//! session-brdy-harness — host-testable seams for android-app/native/src/session.rs
//! (TODO 28: TBUF slot registration + BRDY wiring).
//!
//! The NDK-bound registration (AHardwareBuffer_sendHandleToUnixSocket) cannot
//! run on the host, but the ORDERING contract that the server (lane 18/19)
//! depends on CAN be tested at the transport level:
//!   - P-13: for each blit slot the App must send the length-prefixed TBUF
//!     message and IMMEDIATELY afterwards the raw native_handle bytes (the
//!     handle carries NO u32 length prefix — sendHandleToUnixSocket's wire
//!     output matches the server's raw recv_raw).
//!   - F-14: BRDY (BufferReady) is a plain length-prefixed 16 B message.
//!
//! The helpers below are a VERBATIM copy of the ones in session.rs; if they
//! diverge, this harness is lying. The production seam for the handle write is
//! `AhbSlot::send_registration(fd)` (ahb.rs); here it is an injected closure so
//! the host tests can fake the socket write. Tests reference rule numbers per
//! AGENTS.md.

// Verbatim session.rs helpers are consumed only by this crate's tests.
#![allow(dead_code)]

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use wl_android_common::proto::{self, Message};

// ============================================================================
// Verbatim pure helpers from session.rs (keep in sync!)
// ============================================================================

/// Length-prefix + payload on `wr` (the P-05 wire framing: u32 LE length
/// followed by the message body).
fn write_msg_on(wr: &mut UnixStream, data: &[u8]) -> io::Result<()> {
    let len = (data.len() as u32).to_le_bytes();
    wr.write_all(&len)?;
    wr.write_all(data)?;
    wr.flush()
}

/// F-14: BRDY — signal that blit slot `slot` is ready (pull-model pacing is
/// wired by lane 29; this is just the primitive).
fn send_brdy_on(wr: &mut UnixStream, slot: u32) -> io::Result<()> {
    let brdy = proto::BufferReady::new(slot);
    write_msg_on(wr, &proto::encode(&Message::Ready(brdy)))
}

/// P-13 ordering contract, one slot at a time: the length-prefixed TBUF
/// message first, then the raw native_handle bytes via `handle_sender`
/// (production: `AhbSlot::send_registration(fd)` →
/// `AHardwareBuffer_sendHandleToUnixSocket`, whose output has NO length
/// prefix — it matches the server's raw `recv_raw`). The handle must never
/// precede the TBUF: the server decodes TBUF and treats the very next bytes
/// as the handle.
fn send_tbuf_then_handle(
    wr: &mut UnixStream,
    tbuf_data: &[u8],
    handle_sender: impl FnOnce(i32) -> Result<(), String>,
) -> io::Result<()> {
    write_msg_on(wr, tbuf_data)?;
    handle_sender(wr.as_raw_fd()).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Server-side reader: one length-prefixed frame (mirrors recv_raw's
    /// framing: 4-byte LE length then the payload).
    fn read_frame(srv: &mut UnixStream) -> io::Result<Vec<u8>> {
        let mut len_buf = [0u8; 4];
        srv.read_exact(&mut len_buf)?;
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut data = vec![0u8; len];
        srv.read_exact(&mut data)?;
        Ok(data)
    }

    /// P-13: the TBUF frame arrives FIRST and decodes; the native_handle
    /// bytes arrive SECOND as raw bytes with no length prefix. If the order
    /// were flipped, the first 4 handle bytes would be misread as a length
    /// prefix and the test would fail on a garbage length / bad magic.
    #[test]
    fn tbuf_precedes_native_handle_on_the_wire() {
        let (mut app, mut srv) = UnixStream::pair().unwrap();

        let tbuf = Message::Slot(proto::SlotBuffer::new(
            0,
            1080,
            2400,
            proto::DRM_FORMAT_ABGR8888,
            4352,
        ));
        let tbuf_bytes = proto::encode(&tbuf);
        let handle_bytes: Vec<u8> = (0..32u8).map(|i| i.wrapping_mul(7)).collect();

        // Fake stand-in for AhbSlot::send_registration(fd): write the raw
        // handle bytes to the socket, no length prefix.
        send_tbuf_then_handle(&mut app, &tbuf_bytes, |fd| {
            let n = unsafe { libc::write(fd, handle_bytes.as_ptr().cast(), handle_bytes.len()) };
            if n == handle_bytes.len() as isize {
                Ok(())
            } else {
                Err(format!("fake handle write: short write {n}"))
            }
        })
        .expect("send_tbuf_then_handle");

        // First: the TBUF frame, length-prefixed and decodeable.
        let frame = read_frame(&mut srv).unwrap();
        match proto::decode(&frame, vec![]).expect("decode TBUF") {
            Message::Slot(s) => {
                assert_eq!(s.slot, 0);
                assert_eq!(s.width, 1080);
                assert_eq!(s.height, 2400);
                assert_eq!(s.drm_format, proto::DRM_FORMAT_ABGR8888);
                assert_eq!(s.planes[0].stride, 4352);
            }
            other => panic!("expected Slot, got {other:?}"),
        }

        // Second: exactly the raw handle bytes, no prefix.
        let mut got_handle = vec![0u8; handle_bytes.len()];
        srv.read_exact(&mut got_handle).unwrap();
        assert_eq!(got_handle, handle_bytes);
    }

    /// P-13: the ordering helper does NOT write the TBUF when the handle
    /// write fails — no orphaned TBUF with no handle can confuse the server.
    #[test]
    fn handle_failure_aborts_before_any_partial_send_confusion() {
        let (mut app, _srv) = UnixStream::pair().unwrap();
        let tbuf_bytes = proto::encode(&Message::Slot(proto::SlotBuffer::new(
            1, 1080, 2400, proto::DRM_FORMAT_ABGR8888, 4352,
        )));
        let res = send_tbuf_then_handle(&mut app, &tbuf_bytes, |_fd| Err("boom".into()));
        assert!(res.is_err(), "handle failure must surface as an Err");
    }

    /// F-14: BRDY roundtrips as Message::Ready(BufferReady) with the slot and
    /// MAGIC_BRDY intact (16 B, length-prefixed like every message).
    #[test]
    fn brdy_message_roundtrip() {
        let (mut app, mut srv) = UnixStream::pair().unwrap();

        send_brdy_on(&mut app, 2).expect("send_brdy_on");

        let frame = read_frame(&mut srv).unwrap();
        assert_eq!(frame.len(), 16, "BufferReady is 16 B on the wire");
        match proto::decode(&frame, vec![]).expect("decode BRDY") {
            Message::Ready(b) => {
                assert_eq!(b.magic, proto::MAGIC_BRDY);
                assert_eq!(b.slot, 2);
                assert_eq!(b._reserved, 0);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    /// F-14: slot 0 is a legal BRDY target (0-based slot pool), and
    /// consecutive BRDYs preserve order on the stream.
    #[test]
    fn brdy_slot_zero_and_ordering() {
        let (mut app, mut srv) = UnixStream::pair().unwrap();

        send_brdy_on(&mut app, 0).unwrap();
        send_brdy_on(&mut app, 2).unwrap();

        let m0 = proto::decode(&read_frame(&mut srv).unwrap(), vec![]).unwrap();
        let m2 = proto::decode(&read_frame(&mut srv).unwrap(), vec![]).unwrap();
        match (m0, m2) {
            (Message::Ready(a), Message::Ready(b)) => {
                assert_eq!(a.slot, 0);
                assert_eq!(b.slot, 2);
            }
            other => panic!("expected two Ready messages, got {other:?}"),
        }
    }
}

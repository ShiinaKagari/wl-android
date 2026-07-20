use super::*;
use crate::testutil::fd_util::{memfd_fake_dmabuf, FdCountGuard};
use proptest::prelude::*;

// =============================================================================
// P-03: size assertions (already compile-time checked)
// Here we also verify each struct's byte layout matches the DESIGN.md spec.
// =============================================================================

#[test]
fn hello_size_is_16() {
    assert_eq!(size_of::<HelloMessage>(), 16);
}

#[test]
fn config_size_is_32() {
    assert_eq!(size_of::<ConfigMessage>(), 32);
}

#[test]
fn frame_size_is_80() {
    assert_eq!(size_of::<FrameMessage>(), 80);
}

#[test]
fn ack_size_is_16() {
    assert_eq!(size_of::<FrameAck>(), 16);
}

#[test]
fn slot_size_is_64() {
    assert_eq!(size_of::<SlotBuffer>(), 64);
}

#[test]
fn gone_size_is_16() {
    assert_eq!(size_of::<BufferGone>(), 16);
}

#[test]
fn touch_size_is_24() {
    assert_eq!(size_of::<TouchMessage>(), 24);
}

#[test]
fn plane_desc_size_is_8() {
    assert_eq!(size_of::<PlaneDesc>(), 8);
}

// =============================================================================
// P-04: magic constants match wire representation
// =============================================================================

#[test]
fn magic_helo_is_ascii_helo() {
    assert_eq!(MAGIC_HELO, u32::from_le_bytes(*b"HELO"));
}

#[test]
fn magic_conf_is_ascii_conf() {
    assert_eq!(MAGIC_CONF, u32::from_le_bytes(*b"CONF"));
}

#[test]
fn magic_land_is_ascii_land() {
    assert_eq!(MAGIC_LAND, u32::from_le_bytes(*b"LAND"));
}

#[test]
fn magic_fack_is_ascii_fack() {
    assert_eq!(MAGIC_FACK, u32::from_le_bytes(*b"FACK"));
}

#[test]
fn magic_tbuf_is_ascii_tbuf() {
    assert_eq!(MAGIC_TBUF, u32::from_le_bytes(*b"TBUF"));
}

#[test]
fn magic_bgon_is_ascii_bgon() {
    assert_eq!(MAGIC_BGON, u32::from_le_bytes(*b"BGON"));
}

#[test]
fn magic_touc_is_ascii_touc() {
    assert_eq!(MAGIC_TOUC, u32::from_le_bytes(*b"TOUC"));
}

// =============================================================================
// golden bytes: each message type encoded → stable snapshot (insta)
// P-03 precise byte layout
// =============================================================================

#[test]
fn golden_hello() {
    let msg = HelloMessage::default();
    let bytes = encode(&Message::Hello(msg));
    assert_eq!(bytes.len(), 16);
    assert_eq!(bytes[0..4], *b"HELO");
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), PROTOCOL_VERSION);
    assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), SERVER_CAP_BLIT);
    assert_eq!(u32::from_le_bytes(bytes[12..16].try_into().unwrap()), 0);
    insta::assert_debug_snapshot!(&bytes);
}

#[test]
fn golden_config() {
    let msg = ConfigMessage::new(3392, 2400, 144000, 289, APP_CAP_DIRECT_IMPORT);
    let bytes = encode(&Message::Config(msg));
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[0..4], *b"CONF");
    insta::assert_debug_snapshot!(&bytes);
}

#[test]
fn golden_frame() {
    let fd = memfd_fake_dmabuf(3392 * 2400 * 4);
    let mut msg = FrameMessage {
        magic: MAGIC_LAND,
        num_planes: 1,
        serial: 42,
        modifier: DRM_FORMAT_MOD_LINEAR,
        width: 3392,
        height: 2400,
        drm_format: DRM_FORMAT_ABGR8888,
        flags: FRAME_CARRIES_FDS,
        buffer_id: 1,
        _reserved: 0,
        planes: [
            PlaneDesc { offset: 0, stride: 3392 * 4 },
            PlaneDesc { offset: 0, stride: 0 },
            PlaneDesc { offset: 0, stride: 0 },
            PlaneDesc { offset: 0, stride: 0 },
        ],
    };
    assert!(msg.carries_fds());
    msg.set_carries_fds(false);
    assert!(!msg.carries_fds());
    msg.set_carries_fds(true);

    let bytes = encode(&Message::Frame(msg, vec![fd]));
    assert_eq!(bytes.len(), 80);
    assert_eq!(bytes[0..4], *b"LAND");
    assert_eq!(fd_count(&Message::Frame(msg, vec![])), 1);
    insta::assert_debug_snapshot!(&bytes);
}

#[test]
fn golden_ack() {
    let msg = FrameAck::new(42);
    let bytes = encode(&Message::Ack(msg));
    assert_eq!(bytes.len(), 16);
    assert_eq!(bytes[0..4], *b"FACK");
    insta::assert_debug_snapshot!(&bytes);
}

#[test]
fn golden_slot() {
    let msg = SlotBuffer::new(0, 3392, 2400, DRM_FORMAT_ABGR8888, 3392 * 4);
    let bytes = encode(&Message::Slot(msg));
    assert_eq!(bytes.len(), 64);
    assert_eq!(bytes[0..4], *b"TBUF");
    insta::assert_debug_snapshot!(&bytes);
}

#[test]
fn golden_gone() {
    let msg = BufferGone::new(1);
    let bytes = encode(&Message::Gone(msg));
    assert_eq!(bytes.len(), 16);
    assert_eq!(bytes[0..4], *b"BGON");
    insta::assert_debug_snapshot!(&bytes);
}

#[test]
fn golden_touch() {
    let msg = TouchMessage::new(0, 0.5, 0.75, TOUCH_PHASE_DOWN, 1000);
    let bytes = encode(&Message::Touch(msg));
    assert_eq!(bytes.len(), 24);
    assert_eq!(bytes[0..4], *b"TOUC");
    insta::assert_debug_snapshot!(&bytes);
}

// =============================================================================
// P-05: roundtrip encode → decode (proptest)
// =============================================================================

fn decode_roundtrip(msg: &Message) -> Result<Message, ProtoError> {
    let bytes = encode(msg);
    let fds: Vec<_> = match msg {
        Message::Frame(m, fds) if m.carries_fds() => fds
            .iter()
            .map(|fd| fd.try_clone().unwrap())
            .collect(),
        _ => vec![],
    };
    decode(&bytes, fds)
}

proptest! {
    #[test]
    fn rt_config(width in 100u32..8000, height in 100u32..8000, refresh in 1000u32..240000, dpi in 96u32..600, caps in 0u32..1) {
        let msg = Message::Config(ConfigMessage::new(width, height, refresh, dpi, caps));
        let got = decode_roundtrip(&msg)?;
        assert_eq!(got, msg);
    }

    #[test]
    fn rt_ack(serial in 0u64..) {
        let msg = Message::Ack(FrameAck::new(serial));
        let got = decode_roundtrip(&msg)?;
        assert_eq!(got, msg);
    }

    #[test]
    fn rt_gone(buffer_id in 0u32..1024) {
        let msg = Message::Gone(BufferGone::new(buffer_id));
        let got = decode_roundtrip(&msg)?;
        assert_eq!(got, msg);
    }

    #[test]
    fn rt_touch(touch_id in -10i32..10, x in 0.0f32..1.0, y in 0.0f32..1.0, phase in 0u32..5, time_ms in 0u32..) {
        let msg = Message::Touch(TouchMessage::new(touch_id, x, y, phase, time_ms));
        let got = decode_roundtrip(&msg)?;
        assert_eq!(got, msg);
    }

    #[test]
    fn rt_frame(
        num_planes in 1u32..=4u32,
        serial in 0u64..,
        modifier in 0u64..,
        width in 100u32..8000,
        height in 100u32..8000,
        format in 0u32..,
        flags in 0u32..2,
        buffer_id in 0u32..1024,
        offset0 in 0u32..65535, stride0 in 256u32..65535,
        offset1 in 0u32..65535, stride1 in 0u32..65535,
        offset2 in 0u32..65535, stride2 in 0u32..65535,
        offset3 in 0u32..65535, stride3 in 0u32..65535,
    ) {
        let mut msg = FrameMessage {
            magic: MAGIC_LAND,
            num_planes,
            serial,
            modifier,
            width,
            height,
            drm_format: format,
            flags,
            buffer_id,
            _reserved: 0,
            planes: [
                PlaneDesc { offset: offset0, stride: stride0 },
                PlaneDesc { offset: offset1, stride: stride1 },
                PlaneDesc { offset: offset2, stride: stride2 },
                PlaneDesc { offset: offset3, stride: stride3 },
            ],
        };

        // For rt, always carry fds if num_planes > 0
        if num_planes > 0 {
            msg.flags |= FRAME_CARRIES_FDS;
        }

        let fds: Vec<_> = (0..num_planes as usize)
            .map(|_| memfd_fake_dmabuf(1024))
            .collect();

        let _bytes = encode(&Message::Frame(msg, vec![]));
        let msg_with_fds = Message::Frame(msg, fds);
        let got = decode_roundtrip(&msg_with_fds)?;

        // Compare fields (ignore fds which are just memfds)
        match &got {
            Message::Frame(got_m, _) => {
                assert_eq!(got_m.magic, msg.magic);
                assert_eq!(got_m.serial, msg.serial);
                assert_eq!(got_m.modifier, msg.modifier);
                assert_eq!(got_m.width, msg.width);
                assert_eq!(got_m.height, msg.height);
                assert_eq!(got_m.drm_format, msg.drm_format);
                assert_eq!(got_m.flags, msg.flags);
                assert_eq!(got_m.buffer_id, msg.buffer_id);
                assert_eq!(got_m.num_planes, msg.num_planes);
                for i in 0..4 {
                    assert_eq!(got_m.planes[i], msg.planes[i]);
                }
            }
            _ => panic!("expected Frame"),
        }
    }

    #[test]
    fn rt_slot(
        slot in 0u32..3,
        width in 100u32..8000,
        height in 100u32..8000,
        format in 0u32..,
        stride in 256u32..65535,
    ) {
        let msg = Message::Slot(SlotBuffer::new(slot, width, height, format, stride));
        let got = decode_roundtrip(&msg)?;
        assert_eq!(got, msg);
    }
}

// =============================================================================
// P-05: bad magic
// =============================================================================

#[test]
fn decode_bad_magic() {
    let buf = [0xFF, 0xFF, 0xFF, 0xFF, 0u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let err = decode(&buf, vec![]).unwrap_err();
    assert_eq!(err, ProtoError::BadMagic { got: 0xFFFFFFFF });
}

// =============================================================================
// P-05: bad length
// =============================================================================

#[test]
fn decode_bad_length() {
    let buf = [0u8; 3]; // too short for even a magic
    let err = decode(&buf, vec![]).unwrap_err();
    assert_eq!(err, ProtoError::BadLength { expected: 4, got: 3 });
}

#[test]
fn decode_truncated_frame() {
    let mut buf = [0u8; 40]; // LAND needs 80 bytes
    buf[0..4].copy_from_slice(b"LAND");
    let err = decode(&buf, vec![]).unwrap_err();
    assert_eq!(err, ProtoError::BadLength { expected: 80, got: 40 });
}

// =============================================================================
// P-08, P-09: FD counts in Frame
// =============================================================================

#[test]
fn frame_carries_fds_reports_plane_count() {
    let fd = memfd_fake_dmabuf(1024);
    let msg = Message::Frame(
        FrameMessage {
            magic: MAGIC_LAND,
            num_planes: 2,
            serial: 1,
            modifier: 0,
            width: 1920,
            height: 1080,
            drm_format: 0,
            flags: FRAME_CARRIES_FDS,
            buffer_id: 1,
            _reserved: 0,
            planes: [PlaneDesc { offset: 0, stride: 0 }; 4],
        },
        vec![fd],
    );
    assert_eq!(fd_count(&msg), 2);
}

#[test]
fn frame_without_fds_reports_zero() {
    let msg = Message::Frame(
        FrameMessage {
            magic: MAGIC_LAND,
            num_planes: 2,
            serial: 1,
            modifier: 0,
            width: 1920,
            height: 1080,
            drm_format: 0,
            flags: 0,
            buffer_id: 1,
            _reserved: 0,
            planes: [PlaneDesc { offset: 0, stride: 0 }; 4],
        },
        vec![],
    );
    assert_eq!(fd_count(&msg), 0);
}

// =============================================================================
// F-03: FdCountGuard in roundtrip — no leak on proper roundtrip
// =============================================================================

#[test]
#[ignore = "requires --test-threads=1 due to global fd counting via /proc/self/fd"]
fn fd_count_guard_no_leak_roundtrip() {
    let guard = FdCountGuard::new("fd-no-leak-rt");
    let fd = memfd_fake_dmabuf(1024);
    let msg = Message::Frame(
        FrameMessage {
            magic: MAGIC_LAND,
            num_planes: 1,
            serial: 1,
            modifier: 0,
            width: 1920,
            height: 1080,
            drm_format: 0,
            flags: FRAME_CARRIES_FDS,
            buffer_id: 1,
            _reserved: 0,
            planes: [PlaneDesc { offset: 0, stride: 0 }; 4],
        },
        vec![fd],
    );
    let decoded = decode_roundtrip(&msg).unwrap();
    drop(decoded);
    drop(msg);
    drop(guard);
}

// =============================================================================
// T-01 / T-02 / T-03: touch phases
// =============================================================================

#[test]
fn touch_phases_are_distinct() {
    let phases = [TOUCH_PHASE_DOWN, TOUCH_PHASE_MOVE, TOUCH_PHASE_UP, TOUCH_PHASE_CANCEL, TOUCH_PHASE_FRAME];
    for i in 0..phases.len() {
        for j in (i + 1)..phases.len() {
            assert_ne!(phases[i], phases[j]);
        }
    }
}

// =============================================================================
// Insta snapshot acceptance test runner (for `cargo test --review`)
// =============================================================================

#[test]
fn insta_snapshots_are_fresh() {
    // When golden tests create new snapshots, they need review.
    // This test is always green; insta panics on mismatch during `cargo test`.
    // Run `cargo insta review` to accept changes.
}

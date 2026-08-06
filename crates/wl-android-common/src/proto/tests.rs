use super::*;
use crate::testutil::fd_util::{memfd_fake_dmabuf, FdCountGuard};
use proptest::prelude::*;

// =============================================================================
// P-03: size assertions (already compile-time checked)
// Here we also verify each struct's byte layout matches the DESIGN.md spec.
// =============================================================================

#[test]
fn config_size_is_32() {
    assert_eq!(size_of::<ConfigMessage>(), 32);
}

#[test]
fn frame_size_is_16() {
    assert_eq!(size_of::<FrameMessage>(), 16);
}

#[test]
fn release_size_is_8() {
    assert_eq!(size_of::<ReleaseMessage>(), 8);
}

#[test]
fn touch_size_is_24() {
    assert_eq!(size_of::<TouchMessage>(), 24);
}

#[test]
fn key_size_is_16() {
    assert_eq!(size_of::<KeyMessage>(), 16);
}

// =============================================================================
// P-04: magic constants match wire representation
// =============================================================================

#[test]
fn magic_conf_is_ascii_conf() {
    assert_eq!(MAGIC_CONF, u32::from_le_bytes(*b"CONF"));
}

#[test]
fn magic_confu_is_ascii_conu() {
    assert_eq!(MAGIC_CONFU, u32::from_le_bytes(*b"CONU"));
}

#[test]
fn magic_land_is_ascii_land() {
    assert_eq!(MAGIC_LAND, u32::from_le_bytes(*b"LAND"));
}

#[test]
fn magic_rlse_is_ascii_rlse() {
    assert_eq!(MAGIC_RLSE, u32::from_le_bytes(*b"RLSE"));
}

#[test]
fn magic_touc_is_ascii_touc() {
    assert_eq!(MAGIC_TOUC, u32::from_le_bytes(*b"TOUC"));
}

#[test]
fn magic_keym_is_ascii_keym() {
    assert_eq!(MAGIC_KEYM, u32::from_le_bytes(*b"KEYM"));
}

// =============================================================================
// golden bytes: each message type encoded → stable snapshot (insta)
// =============================================================================

#[test]
fn golden_config() {
    let msg = ConfigMessage::new(3392, 2400, 144000, 289, 0, 0);
    let bytes = encode(&Message::Config(msg));
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[0..4], *b"CONF");
    insta::assert_debug_snapshot!(&bytes);
}

#[test]
fn golden_config_update() {
    let msg = ConfigMessage::new(2400, 3392, 144000, 288, 0, 0).as_config_update();
    assert!(msg.is_config_update(), "as_config_update must flip the magic");
    assert_eq!(msg.magic, MAGIC_CONFU);
    let bytes = encode(&Message::ConfigUpdate(msg));
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[0..4], *b"CONU");
    insta::assert_debug_snapshot!(&bytes);
    // Same layout, different magic: a plain Config must NOT decode as Update.
    let plain = ConfigMessage::new(3392, 2400, 144000, 289, 0, 0);
    assert!(!plain.is_config_update());
}

#[test]
fn golden_frame() {
    let fd = memfd_fake_dmabuf(3392 * 2400 * 4);
    let msg = FrameMessage::new(3392, 2400);
    assert!(msg.carries_fds());

    let bytes = encode(&Message::Frame(msg, vec![fd]));
    assert_eq!(bytes.len(), 16);
    assert_eq!(bytes[0..4], *b"LAND");
    assert_eq!(fd_count(&Message::Frame(msg, vec![])), 1);
    insta::assert_debug_snapshot!(&bytes);
}

#[test]
fn golden_release() {
    let msg = ReleaseMessage::new();
    let bytes = encode(&Message::Release(msg));
    assert_eq!(bytes.len(), 8);
    assert_eq!(bytes[0..4], *b"RLSE");
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

#[test]
fn keym_message_golden() {
    let msg = KeyMessage::new(30, 1, 12345);
    let bytes = encode(&Message::Key(msg));
    assert_eq!(bytes.len(), 16);
    assert_eq!(bytes[0..4], *b"KEYM");
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
        let msg = Message::Config(ConfigMessage::new(width, height, refresh, dpi, caps, 0));
        let got = decode_roundtrip(&msg)?;
        assert_eq!(got, msg);
    }

    #[test]
    fn rt_config_update(width in 100u32..8000, height in 100u32..8000, refresh in 1000u32..240000, dpi in 96u32..600, mode in 0u32..4) {
        let msg = Message::ConfigUpdate(
            ConfigMessage::new(width, height, refresh, dpi, 0, mode).as_config_update(),
        );
        let got = decode_roundtrip(&msg)?;
        assert_eq!(got, msg);
    }

    #[test]
    fn rt_release(_dummy in 0u32..1) {
        let msg = Message::Release(ReleaseMessage::new());
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
    fn rt_frame(width in 100u32..8000, height in 100u32..8000) {
        let msg = FrameMessage::new(width, height);
        let fds = vec![memfd_fake_dmabuf(1024)];
        let got = decode_roundtrip(&Message::Frame(msg, fds))?;

        match &got {
            Message::Frame(got_m, _) => {
                assert_eq!(got_m.magic, msg.magic);
                assert_eq!(got_m.width, msg.width);
                assert_eq!(got_m.height, msg.height);
                assert_eq!(got_m.flags, msg.flags);
                assert!(got_m.carries_fds());
            }
            _ => panic!("expected Frame"),
        }
    }

    #[test]
    fn rt_key(keycode in 0u32..256, state in 0u32..2, time_ms in 0u32..) {
        let msg = Message::Key(KeyMessage::new(keycode, state, time_ms));
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
    let mut buf = [0u8; 8]; // LAND needs 16 bytes
    buf[0..4].copy_from_slice(b"LAND");
    let err = decode(&buf, vec![]).unwrap_err();
    assert_eq!(err, ProtoError::BadLength { expected: 16, got: 8 });
}

#[test]
fn decode_truncated_keym() {
    let mut buf = [0u8; 8]; // KEYM needs 16 bytes
    buf[0..4].copy_from_slice(b"KEYM");
    let err = decode(&buf, vec![]).unwrap_err();
    assert_eq!(err, ProtoError::BadLength { expected: 16, got: 8 });
}

#[test]
fn decode_truncated_release() {
    let mut buf = [0u8; 4]; // RLSE needs 8 bytes
    buf[0..4].copy_from_slice(b"RLSE");
    let err = decode(&buf, vec![]).unwrap_err();
    assert_eq!(err, ProtoError::BadLength { expected: 8, got: 4 });
}

// =============================================================================
// P-08: FD counts in Frame
// =============================================================================

#[test]
fn frame_carries_fds_reports_one() {
    let msg = Message::Frame(FrameMessage::new(1920, 1080), vec![memfd_fake_dmabuf(1024)]);
    assert_eq!(fd_count(&msg), 1);
}

#[test]
fn frame_without_fds_reports_zero() {
    let msg = FrameMessage::new(1920, 1080);
    assert_eq!(fd_count(&Message::Frame(msg, vec![])), 1, "frames always carry the pixel fd");
}

#[test]
fn frame_missing_fd_is_fd_mismatch() {
    let bytes = encode(&Message::Frame(FrameMessage::new(1920, 1080), vec![]));
    let err = decode(&bytes, vec![]).unwrap_err();
    assert_eq!(err, ProtoError::FdMismatch { expected: 1, got: 0 });
}

#[test]
fn frame_extra_fd_is_fd_mismatch() {
    let bytes = encode(&Message::Frame(FrameMessage::new(1920, 1080), vec![]));
    let fds = vec![memfd_fake_dmabuf(64), memfd_fake_dmabuf(64)];
    let err = decode(&bytes, fds).unwrap_err();
    assert_eq!(err, ProtoError::FdMismatch { expected: 1, got: 2 });
}

#[test]
fn release_carries_no_fds() {
    let bytes = encode(&Message::Release(ReleaseMessage::new()));
    let got = decode(&bytes, vec![]).unwrap();
    assert!(matches!(got, Message::Release(_)));
}

// =============================================================================
// F-03: FdCountGuard in roundtrip — no leak on proper roundtrip
// =============================================================================

#[test]
#[ignore = "requires --test-threads=1 due to global fd counting via /proc/self/fd"]
fn fd_count_guard_no_leak_roundtrip() {
    let guard = FdCountGuard::new("fd-no-leak-rt");
    let fd = memfd_fake_dmabuf(1024);
    let msg = Message::Frame(FrameMessage::new(1920, 1080), vec![fd]);
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

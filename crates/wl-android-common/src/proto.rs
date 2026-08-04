use std::os::unix::io::OwnedFd;

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// =============================================================================
// Magic constants (§4, P-04)
// =============================================================================

pub const MAGIC_HELO: u32 = u32::from_le_bytes(*b"HELO");
pub const MAGIC_CONF: u32 = u32::from_le_bytes(*b"CONF");
pub const MAGIC_LAND: u32 = u32::from_le_bytes(*b"LAND");
pub const MAGIC_FACK: u32 = u32::from_le_bytes(*b"FACK");
pub const MAGIC_TBUF: u32 = u32::from_le_bytes(*b"TBUF");
pub const MAGIC_BGON: u32 = u32::from_le_bytes(*b"BGON");
pub const MAGIC_TOUC: u32 = u32::from_le_bytes(*b"TOUC");
pub const MAGIC_KEYM: u32 = u32::from_le_bytes(*b"KEYM");
pub const MAGIC_BRDY: u32 = u32::from_le_bytes(*b"BRDY");

// =============================================================================
// Version
// =============================================================================

pub const PROTOCOL_VERSION: u32 = 2;

// =============================================================================
// Caps bits
// =============================================================================

pub const SERVER_CAP_BLIT: u32 = 1 << 0;
/// Server is in SHM/CPU fallback mode (LAND_MODE=shm): frames carry pixel
/// fds (no fence), the App presents via the CPU path and must NOT init the
/// Vulkan swapchain (CPU lock + Vulkan swapchain on one ANativeWindow
/// conflict — ANativeWindow_lock returns -22).
pub const SERVER_CAP_SHM: u32 = 1 << 1;
pub const SERVER_CAP_FENCE: u32 = 1 << 1;

pub const APP_CAP_DIRECT_IMPORT: u32 = 1 << 0;
pub const APP_CAP_SWAPCHAIN: u32 = 1 << 1;

// =============================================================================
// Frame flags
// =============================================================================

pub const FRAME_CARRIES_FDS: u32 = 1 << 0;
pub const FRAME_CARRIES_FENCE: u32 = 1 << 1;

// =============================================================================
// Touch phases
// =============================================================================

pub const TOUCH_PHASE_DOWN: u32 = 0;
pub const TOUCH_PHASE_MOVE: u32 = 1;
pub const TOUCH_PHASE_UP: u32 = 2;
pub const TOUCH_PHASE_CANCEL: u32 = 3;
pub const TOUCH_PHASE_FRAME: u32 = 4;

// =============================================================================
// Limits
// =============================================================================

pub const MAX_IN_FLIGHT: usize = 2;
pub const SLOT_COUNT: usize = 3;
pub const MAX_PLANES: usize = 4;

// =============================================================================
// DRM constants (not from a lib yet, inlined for now)
// =============================================================================

pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
pub const DRM_FORMAT_MOD_QCOM_COMPRESSED: u64 = 0x0800_0000_0000_0005;
pub const DRM_FORMAT_XRGB8888: u32 = fourcc(b"XR24");
pub const DRM_FORMAT_ARGB8888: u32 = fourcc(b"AR24");
pub const DRM_FORMAT_XBGR8888: u32 = fourcc(b"XB24");
pub const DRM_FORMAT_ABGR8888: u32 = fourcc(b"AB24");

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}

// =============================================================================
// Plane descriptor (sub-struct reused in Frame and TBUF) — P-03
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct PlaneDesc {
    pub offset: u32,
    pub stride: u32,
}

// =============================================================================
// 4.1 HelloMessage (server → App, 16 B) — P-01..P-03
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct HelloMessage {
    pub magic: u32,
    pub protocol_version: u32,
    pub server_caps: u32,
    pub _reserved: u32,
}

impl Default for HelloMessage {
    fn default() -> Self {
        Self {
            magic: MAGIC_HELO,
            protocol_version: PROTOCOL_VERSION,
            server_caps: SERVER_CAP_BLIT,
            _reserved: 0,
        }
    }
}

// =============================================================================
// 4.2 ConfigMessage (App → server, 32 B)
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct ConfigMessage {
    pub magic: u32,
    pub protocol_version: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
    pub dpi: u32,
    pub app_caps: u32,
    pub _reserved: u32,
}

impl ConfigMessage {
    pub fn new(width: u32, height: u32, refresh_millihz: u32, dpi: u32, app_caps: u32) -> Self {
        Self {
            magic: MAGIC_CONF,
            protocol_version: PROTOCOL_VERSION,
            width,
            height,
            refresh_millihz,
            dpi,
            app_caps,
            _reserved: 0,
        }
    }
}

// =============================================================================
// 4.3 FrameMessage (server → App, 80 B) — P-08/P-09
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct FrameMessage {
    pub magic: u32,
    pub num_planes: u32,
    pub serial: u64,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    pub drm_format: u32,
    pub flags: u32,
    pub buffer_id: u32,
    pub _reserved: u32,
    pub planes: [PlaneDesc; 4],
}

impl FrameMessage {
    pub fn carries_fds(&self) -> bool {
        self.flags & FRAME_CARRIES_FDS != 0
    }

    pub fn set_carries_fds(&mut self, v: bool) {
        if v {
            self.flags |= FRAME_CARRIES_FDS;
        } else {
            self.flags &= !FRAME_CARRIES_FDS;
        }
    }

    pub fn carries_fence(&self) -> bool {
        self.flags & FRAME_CARRIES_FENCE != 0
    }

    pub fn set_carries_fence(&mut self, v: bool) {
        if v {
            self.flags |= FRAME_CARRIES_FENCE;
        } else {
            self.flags &= !FRAME_CARRIES_FENCE;
        }
    }
}

// =============================================================================
// 4.4 FrameAck (App → server, 16 B) — P-11
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct FrameAck {
    pub magic: u32,
    pub _reserved: u32,
    pub serial: u64,
}

impl FrameAck {
    pub fn new(serial: u64) -> Self {
        Self {
            magic: MAGIC_FACK,
            _reserved: 0,
            serial,
        }
    }
}

// =============================================================================
// 4.5 SlotBuffer (App → server, 64 B) — P-12/P-13
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct SlotBuffer {
    pub magic: u32,
    pub slot: u32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    pub drm_format: u32,
    pub num_planes: u32,
    pub planes: [PlaneDesc; 4],
}

impl SlotBuffer {
    pub fn new(slot: u32, width: u32, height: u32, drm_format: u32, stride_bytes: u32) -> Self {
        Self {
            magic: MAGIC_TBUF,
            slot,
            modifier: DRM_FORMAT_MOD_LINEAR,
            width,
            height,
            drm_format,
            num_planes: 1,
            planes: [
                PlaneDesc { offset: 0, stride: stride_bytes },
                PlaneDesc { offset: 0, stride: 0 },
                PlaneDesc { offset: 0, stride: 0 },
                PlaneDesc { offset: 0, stride: 0 },
            ],
        }
    }
}

// =============================================================================
// 4.6 BufferGone (server → App, 16 B) — P-15
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct BufferGone {
    pub magic: u32,
    pub buffer_id: u32,
    pub _reserved: u64,
}

impl BufferGone {
    pub fn new(buffer_id: u32) -> Self {
        Self {
            magic: MAGIC_BGON,
            buffer_id,
            _reserved: 0,
        }
    }
}

// =============================================================================
// 4.7 TouchMessage (App → server, 24 B) — T-01..T-03
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct TouchMessage {
    pub magic: u32,
    pub touch_id: i32,
    pub x: f32,
    pub y: f32,
    pub phase: u32,
    pub time_ms: u32,
}

impl TouchMessage {
    pub fn new(touch_id: i32, x: f32, y: f32, phase: u32, time_ms: u32) -> Self {
        Self {
            magic: MAGIC_TOUC,
            touch_id,
            x,
            y,
            phase,
            time_ms,
        }
    }
}

// =============================================================================
// 4.8 KeyMessage (App → server, 16 B) — T-04
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct KeyMessage {
    pub magic: u32,
    pub keycode: u32,
    pub state: u32,
    pub time_ms: u32,
}

impl KeyMessage {
    pub fn new(keycode: u32, state: u32, time_ms: u32) -> Self {
        Self {
            magic: MAGIC_KEYM,
            keycode,
            state,
            time_ms,
        }
    }
}

// =============================================================================
// 4.9 BufferReady (App → server, 16 B) — F-14
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct BufferReady {
    pub magic: u32,
    pub slot: u32,
    pub _reserved: u64,
}

impl BufferReady {
    pub fn new(slot: u32) -> Self {
        Self {
            magic: MAGIC_BRDY,
            slot,
            _reserved: 0,
        }
    }
}

// =============================================================================
// Message enum (decode output)
// =============================================================================

#[derive(Debug)]
pub enum Message {
    Hello(HelloMessage),
    Config(ConfigMessage),
    Frame(FrameMessage, Vec<OwnedFd>),
    Ack(FrameAck),
    Slot(SlotBuffer),
    Gone(BufferGone),
    Touch(TouchMessage),
    Key(KeyMessage),
    Ready(BufferReady),
}

impl PartialEq for Message {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Hello(a), Self::Hello(b)) => a == b,
            (Self::Config(a), Self::Config(b)) => a == b,
            (Self::Frame(a, fa), Self::Frame(b, fb)) => a == b && fa.len() == fb.len(),
            (Self::Ack(a), Self::Ack(b)) => a == b,
            (Self::Slot(a), Self::Slot(b)) => a == b,
            (Self::Gone(a), Self::Gone(b)) => a == b,
            (Self::Touch(a), Self::Touch(b)) => a == b,
            (Self::Key(a), Self::Key(b)) => a == b,
            (Self::Ready(a), Self::Ready(b)) => a == b,
            _ => false,
        }
    }
}

// =============================================================================
// Error type
// =============================================================================

#[derive(Debug, PartialEq, Eq)]
pub enum ProtoError {
    BadMagic { got: u32 },
    BadLength { expected: usize, got: usize },
    FdMismatch { expected: usize, got: usize },
}

impl core::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadMagic { got } => write!(f, "bad magic: 0x{got:08X}"),
            Self::BadLength { expected, got } => {
                write!(f, "bad message length: expected {expected}, got {got}")
            }
            Self::FdMismatch { expected, got } => {
                write!(f, "fd count mismatch: expected {expected}, got {got}")
            }
        }
    }
}

impl std::error::Error for ProtoError {}

// =============================================================================
// Size assertions (compile-time) — P-03
// =============================================================================

const _: () = {
    assert!(size_of::<HelloMessage>() == 16);
    assert!(size_of::<ConfigMessage>() == 32);
    assert!(size_of::<FrameMessage>() == 80);
    assert!(size_of::<FrameAck>() == 16);
    assert!(size_of::<SlotBuffer>() == 64);
    assert!(size_of::<BufferGone>() == 16);
    assert!(size_of::<TouchMessage>() == 24);
    assert!(size_of::<KeyMessage>() == 16);
    assert!(size_of::<BufferReady>() == 16);
    assert!(size_of::<PlaneDesc>() == 8);
};

// =============================================================================
// Encode / Decode — P-05
// =============================================================================

/// Produces the wire bytes. The caller must supply the fds to accompany the
/// message (if any) when calling Transport::send.
pub fn encode(msg: &Message) -> Vec<u8> {
    match msg {
        Message::Hello(m) => m.as_bytes().to_vec(),
        Message::Config(m) => m.as_bytes().to_vec(),
        Message::Frame(m, _) => m.as_bytes().to_vec(),
        Message::Ack(m) => m.as_bytes().to_vec(),
        Message::Slot(m) => m.as_bytes().to_vec(),
        Message::Gone(m) => m.as_bytes().to_vec(),
        Message::Touch(m) => m.as_bytes().to_vec(),
        Message::Key(m) => m.as_bytes().to_vec(),
        Message::Ready(m) => m.as_bytes().to_vec(),
    }
}

/// Returns the number of fds that must accompany this message via SCM_RIGHTS.
pub fn fd_count(msg: &Message) -> usize {
    match msg {
        // P-08: CARRIES_FDS → num_planes fds
        // P-08b: CARRIES_FENCE → +1 fence fd (coexistable)
        Message::Frame(m, _) => {
            (if m.carries_fds() { m.num_planes as usize } else { 0 })
                + (if m.carries_fence() { 1 } else { 0 })
        }
        _ => 0,
    }
}

/// Decode bytes + received fds into a Message.
pub fn decode(buf: &[u8], fds: Vec<OwnedFd>) -> Result<Message, ProtoError> {
    if buf.len() < 4 {
        return Err(ProtoError::BadLength { expected: 4, got: buf.len() });
    }
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);

    match magic {
        MAGIC_HELO => {
            check_len::<HelloMessage>(buf)?;
            let (m, _) = HelloMessage::read_from_prefix(buf).unwrap();
            Ok(Message::Hello(m))
        }
        MAGIC_CONF => {
            check_len::<ConfigMessage>(buf)?;
            let (m, _) = ConfigMessage::read_from_prefix(buf).unwrap();
            Ok(Message::Config(m))
        }
        MAGIC_LAND => {
            check_len::<FrameMessage>(buf)?;
            let (m, _) = FrameMessage::read_from_prefix(buf).unwrap();
            let expected = (if m.carries_fds() { m.num_planes as usize } else { 0 })
                + (if m.carries_fence() { 1 } else { 0 });
            if fds.len() != expected {
                return Err(ProtoError::FdMismatch { expected, got: fds.len() });
            }
            Ok(Message::Frame(m, fds))
        }
        MAGIC_FACK => {
            check_len::<FrameAck>(buf)?;
            let (m, _) = FrameAck::read_from_prefix(buf).unwrap();
            Ok(Message::Ack(m))
        }
        MAGIC_TBUF => {
            check_len::<SlotBuffer>(buf)?;
            let (m, _) = SlotBuffer::read_from_prefix(buf).unwrap();
            Ok(Message::Slot(m))
        }
        MAGIC_BGON => {
            check_len::<BufferGone>(buf)?;
            let (m, _) = BufferGone::read_from_prefix(buf).unwrap();
            Ok(Message::Gone(m))
        }
        MAGIC_TOUC => {
            check_len::<TouchMessage>(buf)?;
            let (m, _) = TouchMessage::read_from_prefix(buf).unwrap();
            Ok(Message::Touch(m))
        }
        MAGIC_KEYM => {
            check_len::<KeyMessage>(buf)?;
            let (m, _) = KeyMessage::read_from_prefix(buf).unwrap();
            Ok(Message::Key(m))
        }
        MAGIC_BRDY => {
            check_len::<BufferReady>(buf)?;
            let (m, _) = BufferReady::read_from_prefix(buf).unwrap();
            Ok(Message::Ready(m))
        }
        _ => Err(ProtoError::BadMagic { got: magic }),
    }
}

fn check_len<T>(buf: &[u8]) -> Result<(), ProtoError> {
    let expected = size_of::<T>();
    if buf.len() < expected {
        Err(ProtoError::BadLength {
            expected,
            got: buf.len(),
        })
    } else {
        Ok(())
    }
}

// =============================================================================
// Tests — golden bytes + proptest roundtrip
// =============================================================================

#[cfg(test)]
mod tests;

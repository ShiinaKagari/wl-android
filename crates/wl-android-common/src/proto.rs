use std::os::unix::io::OwnedFd;

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// =============================================================================
// Magic constants (§4, P-04)
// =============================================================================

pub const MAGIC_CONF: u32 = u32::from_le_bytes(*b"CONF");
pub const MAGIC_CONFU: u32 = u32::from_le_bytes(*b"CONU");
pub const MAGIC_LAND: u32 = u32::from_le_bytes(*b"LAND");
pub const MAGIC_RLSE: u32 = u32::from_le_bytes(*b"RLSE");
pub const MAGIC_TOUC: u32 = u32::from_le_bytes(*b"TOUC");
pub const MAGIC_KEYM: u32 = u32::from_le_bytes(*b"KEYM");

// =============================================================================
// Version
// =============================================================================

pub const PROTOCOL_VERSION: u32 = 3;

// =============================================================================
// Frame flags
// =============================================================================

pub const FRAME_CARRIES_FDS: u32 = 1 << 0;

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

pub const MAX_PLANES: usize = 4;

// =============================================================================
// DRM constants (not from a lib yet, inlined for now)
// =============================================================================

pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
pub const DRM_FORMAT_XRGB8888: u32 = fourcc(b"XR24");
pub const DRM_FORMAT_ARGB8888: u32 = fourcc(b"AR24");
pub const DRM_FORMAT_XBGR8888: u32 = fourcc(b"XB24");
pub const DRM_FORMAT_ABGR8888: u32 = fourcc(b"AB24");

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}

// =============================================================================
// 4.1 ConfigMessage (App → server, 32 B; server → App as ConfigUpdate, 32 B)
//
// Stateless protocol: CONF is a plain event the App sends whenever the
// display geometry/refresh/DPI/frame-mode changes (no handshake). The server
// applies it and mirrors the effective (DPI-bucketed) values back via CONFU.
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
    /// Frame pacing mode (previously `_reserved`): 0 = free, 1 = vsync-align
    /// (deliver at the refresh period), 2 = performance (no pacing, minimum
    /// buffering), 3 = power-save (cap frame rate below refresh).
    pub frame_mode: u32,
}

impl ConfigMessage {
    pub fn new(width: u32, height: u32, refresh_millihz: u32, dpi: u32, app_caps: u32, frame_mode: u32) -> Self {
        Self {
            magic: MAGIC_CONF,
            protocol_version: PROTOCOL_VERSION,
            width,
            height,
            refresh_millihz,
            dpi,
            app_caps,
            frame_mode,
        }
    }

    /// Server → App variant of the same 32 B layout: signals a display
    /// geometry/DPI/refresh change the App must apply (resize the render
    /// window, update HiDPI scale). `app_caps` carries no meaning here.
    pub fn as_config_update(mut self) -> Self {
        self.magic = MAGIC_CONFU;
        self
    }

    pub fn is_config_update(&self) -> bool {
        self.magic == MAGIC_CONFU
    }
}

// =============================================================================
// 4.2 FrameMessage (server → App, 16 B + 1 pixel fd)
//
// Stateless frame push: the server sends the CURRENT frame whenever KWin
// commits; the App renders it and replies with a ReleaseMessage. There is no
// serial, no ack, no ordering contract — latest-wins on both ends.
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct FrameMessage {
    pub magic: u32,
    pub width: u32,
    pub height: u32,
    pub flags: u32,
}

impl FrameMessage {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            magic: MAGIC_LAND,
            width,
            height,
            flags: FRAME_CARRIES_FDS,
        }
    }

    pub fn carries_fds(&self) -> bool {
        self.flags & FRAME_CARRIES_FDS != 0
    }
}

// =============================================================================
// 4.3 ReleaseMessage (App → server, 8 B)
//
// Buffer-ownership signal: the App has finished consuming the most recently
// received frame's pixel fd and the server may reuse that memfd. No payload
// needed — the server tracks its double-buffer rotation and frees the
// oldest in-flight buffer per release (FIFO), so releases are order-only.
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct ReleaseMessage {
    pub magic: u32,
    pub _reserved: u32,
}

impl ReleaseMessage {
    pub fn new() -> Self {
        Self {
            magic: MAGIC_RLSE,
            _reserved: 0,
        }
    }
}

impl Default for ReleaseMessage {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// 4.5 TouchMessage (App → server, 24 B) — T-01..T-03
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
// Message enum (decode output)
// =============================================================================

#[derive(Debug)]
pub enum Message {
    Config(ConfigMessage),
    /// Server → App display-geometry/DPI/refresh change (same 32 B layout as
    /// ConfigMessage, `MAGIC_CONFU`; see [`ConfigMessage::as_config_update`]).
    ConfigUpdate(ConfigMessage),
    Frame(FrameMessage, Vec<OwnedFd>),
    /// App → server buffer-ownership signal (see [`ReleaseMessage`]).
    Release(ReleaseMessage),
    Touch(TouchMessage),
    Key(KeyMessage),
}

impl PartialEq for Message {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Config(a), Self::Config(b)) => a == b,
            (Self::ConfigUpdate(a), Self::ConfigUpdate(b)) => a == b,
            (Self::Frame(a, fa), Self::Frame(b, fb)) => a == b && fa.len() == fb.len(),
            (Self::Release(a), Self::Release(b)) => a == b,
            (Self::Touch(a), Self::Touch(b)) => a == b,
            (Self::Key(a), Self::Key(b)) => a == b,
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
    assert!(size_of::<ConfigMessage>() == 32);
    assert!(size_of::<FrameMessage>() == 16);
    assert!(size_of::<ReleaseMessage>() == 8);
    assert!(size_of::<TouchMessage>() == 24);
    assert!(size_of::<KeyMessage>() == 16);
};

// =============================================================================
// Encode / Decode — P-05
// =============================================================================

/// Produces the wire bytes. The caller must supply the fds to accompany the
/// message (if any) when calling Transport::send.
pub fn encode(msg: &Message) -> Vec<u8> {
    match msg {
        Message::Config(m) => m.as_bytes().to_vec(),
        Message::ConfigUpdate(m) => m.as_bytes().to_vec(),
        Message::Frame(m, _) => m.as_bytes().to_vec(),
        Message::Release(m) => m.as_bytes().to_vec(),
        Message::Touch(m) => m.as_bytes().to_vec(),
        Message::Key(m) => m.as_bytes().to_vec(),
    }
}

/// Returns the number of fds that must accompany this message via SCM_RIGHTS.
pub fn fd_count(msg: &Message) -> usize {
    match msg {
        Message::Frame(m, _) => {
            if m.carries_fds() { 1 } else { 0 }
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
        MAGIC_CONF => {
            check_len::<ConfigMessage>(buf)?;
            let (m, _) = ConfigMessage::read_from_prefix(buf).unwrap();
            Ok(Message::Config(m))
        }
        MAGIC_CONFU => {
            check_len::<ConfigMessage>(buf)?;
            let (m, _) = ConfigMessage::read_from_prefix(buf).unwrap();
            Ok(Message::ConfigUpdate(m))
        }
        MAGIC_LAND => {
            check_len::<FrameMessage>(buf)?;
            let (m, _) = FrameMessage::read_from_prefix(buf).unwrap();
            let expected = if m.carries_fds() { 1 } else { 0 };
            if fds.len() != expected {
                return Err(ProtoError::FdMismatch { expected, got: fds.len() });
            }
            Ok(Message::Frame(m, fds))
        }
        MAGIC_RLSE => {
            check_len::<ReleaseMessage>(buf)?;
            let (m, _) = ReleaseMessage::read_from_prefix(buf).unwrap();
            Ok(Message::Release(m))
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

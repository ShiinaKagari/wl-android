use std::mem::size_of;

pub const PROTOCOL_MAGIC: u32 = 0x4C414E00;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum MessageType {
    Frame = 0x4C414E01,
    Touch = 0x4C414E02,
    Ack = 0x4C414E03,
}

impl MessageType {
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0x4C414E01 => Some(Self::Frame),
            0x4C414E02 => Some(Self::Touch),
            0x4C414E03 => Some(Self::Ack),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum GestureType {
    TouchDown  = 0,
    TouchMove  = 1,
    TouchUp    = 2,
    Pinch      = 3,
    PinchEnd   = 4,
    Scroll     = 5,
    ScrollEnd  = 6,
}

#[derive(Clone, Debug)]
#[repr(C, packed)]
pub struct MessageHeader {
    pub magic: u32,
    pub msg_type: u32,
    pub length: u32,
}

impl MessageHeader {
    pub fn new(msg_type: MessageType, payload_length: u32) -> Self {
        Self {
            magic: PROTOCOL_MAGIC,
            msg_type: msg_type as u32,
            length: payload_length,
        }
    }

    pub fn validate(&self) -> bool {
        self.magic == PROTOCOL_MAGIC && MessageType::from_u32(self.msg_type).is_some()
    }

    pub const fn serialized_size() -> usize {
        size_of::<Self>()
    }
}

#[derive(Clone, Debug)]
#[repr(C, packed)]
pub struct FrameMessage {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub stride: u32,
    pub serial: u64,
}

impl FrameMessage {
    pub fn new(width: u32, height: u32, format: u32, stride: u32, serial: u64) -> Self {
        Self { width, height, format, stride, serial }
    }

    pub const fn serialized_size() -> usize {
        size_of::<Self>()
    }
}

/// 触摸/手势消息。gesture 字段区分事件类型。
#[derive(Clone, Debug)]
#[repr(C, packed)]
pub struct TouchMessage {
    pub serial: u32,
    pub touch_id: i32,
    pub gesture: u32,          // GestureType
    pub x: f32,
    pub y: f32,
    pub dx: f32,               // 用于 scroll/pinch（增量）
    pub dy: f32,
    pub scale: f32,            // 用于 pinch
    pub pressed: u8,           // 布尔
}

impl TouchMessage {
    pub fn new_touch(serial: u32, touch_id: i32, gesture: GestureType, x: f32, y: f32, pressed: bool) -> Self {
        Self {
            serial, touch_id, gesture: gesture as u32,
            x, y,
            dx: 0.0, dy: 0.0, scale: 1.0,
            pressed: pressed as u8,
        }
    }

    pub fn new_scroll(serial: u32, dx: f32, dy: f32) -> Self {
        Self {
            serial, touch_id: 0, gesture: GestureType::Scroll as u32,
            x: 0.0, y: 0.0,
            dx, dy, scale: 1.0,
            pressed: 1,
        }
    }

    pub fn new_pinch(serial: u32, scale: f32) -> Self {
        Self {
            serial, touch_id: 0, gesture: GestureType::Pinch as u32,
            x: 0.0, y: 0.0,
            dx: 0.0, dy: 0.0, scale,
            pressed: 1,
        }
    }

    pub const fn serialized_size() -> usize {
        size_of::<Self>()
    }
}

#[derive(Clone, Debug)]
#[repr(C, packed)]
pub struct AckMessage {
    pub serial: u64,
}

impl AckMessage {
    pub fn new(serial: u64) -> Self {
        Self { serial }
    }

    pub const fn serialized_size() -> usize {
        size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_validate_ok() {
        let h = MessageHeader::new(MessageType::Frame, 0);
        assert!(h.validate());
    }

    #[test]
    fn header_validate_bad_magic() {
        let h = MessageHeader { magic: 0xDEADBEEF, msg_type: MessageType::Frame as u32, length: 0 };
        assert!(!h.validate());
    }

    #[test]
    fn frame_message_size() {
        assert_eq!(FrameMessage::serialized_size(), 24);
    }

    #[test]
    fn touch_message_size() {
        assert_eq!(TouchMessage::serialized_size(), 33);
    }

    #[test]
    fn ack_message_size() {
        assert_eq!(AckMessage::serialized_size(), 8);
    }

    #[test]
    fn header_size() {
        assert_eq!(MessageHeader::serialized_size(), 12);
    }

    #[test]
    fn enum_values() {
        assert_eq!(MessageType::Frame as u32, 0x4C414E01);
        assert_eq!(MessageType::Touch as u32, 0x4C414E02);
        assert_eq!(MessageType::Ack as u32, 0x4C414E03);
    }

    #[test]
    fn frame_message_fields() {
        let m = FrameMessage::new(1920, 1080, 0x34325258, 7680, 42);
        unsafe {
            assert_eq!(std::ptr::read_unaligned(std::ptr::addr_of!(m.width)), 1920);
            assert_eq!(std::ptr::read_unaligned(std::ptr::addr_of!(m.height)), 1080);
            assert_eq!(std::ptr::read_unaligned(std::ptr::addr_of!(m.format)), 0x34325258);
            assert_eq!(std::ptr::read_unaligned(std::ptr::addr_of!(m.stride)), 7680);
            assert_eq!(std::ptr::read_unaligned(std::ptr::addr_of!(m.serial)), 42);
        }
    }
}

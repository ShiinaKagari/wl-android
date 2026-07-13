use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use land_common::protocol::{GestureType, MessageHeader, MessageType, TouchMessage};

pub struct TouchSender {
    stream: UnixStream,
    serial: u32,
}

impl TouchSender {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Ok(Self { stream, serial: 1 })
    }

    pub fn send_touch_down(&mut self, touch_id: i32, x: f32, y: f32) -> io::Result<()> {
        self.send_touch(TouchMessage::new_touch(self.serial, touch_id, GestureType::TouchDown, x, y, true))
    }

    pub fn send_touch_move(&mut self, touch_id: i32, x: f32, y: f32) -> io::Result<()> {
        self.send_touch(TouchMessage::new_touch(self.serial, touch_id, GestureType::TouchMove, x, y, true))
    }

    pub fn send_touch_up(&mut self, touch_id: i32, x: f32, y: f32) -> io::Result<()> {
        self.send_touch(TouchMessage::new_touch(self.serial, touch_id, GestureType::TouchUp, x, y, false))
    }

    pub fn send_scroll(&mut self, dx: f32, dy: f32) -> io::Result<()> {
        self.send_touch(TouchMessage::new_scroll(self.serial, dx, dy))
    }

    pub fn send_scroll_end(&mut self) -> io::Result<()> {
        let mut msg = TouchMessage::new_scroll(self.serial, 0.0, 0.0);
        msg.gesture = GestureType::ScrollEnd as u32;
        self.send_touch(msg)
    }

    pub fn send_pinch(&mut self, scale: f32) -> io::Result<()> {
        self.send_touch(TouchMessage::new_pinch(self.serial, scale))
    }

    pub fn send_pinch_end(&mut self) -> io::Result<()> {
        let mut msg = TouchMessage::new_pinch(self.serial, 1.0);
        msg.gesture = GestureType::PinchEnd as u32;
        self.send_touch(msg)
    }

    fn send_touch(&mut self, touch: TouchMessage) -> io::Result<()> {
        let header = MessageHeader::new(MessageType::Touch, TouchMessage::serialized_size() as u32);

        let header_bytes = unsafe {
            std::slice::from_raw_parts(
                &header as *const MessageHeader as *const u8,
                std::mem::size_of::<MessageHeader>(),
            )
        };
        let touch_bytes = unsafe {
            std::slice::from_raw_parts(
                &touch as *const TouchMessage as *const u8,
                std::mem::size_of::<TouchMessage>(),
            )
        };

        let mut buf = Vec::with_capacity(header_bytes.len() + touch_bytes.len());
        buf.extend_from_slice(header_bytes);
        buf.extend_from_slice(touch_bytes);

        self.stream.write_all(&buf)?;
        self.serial = self.serial.wrapping_add(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_touch_no_socket() {
        let sender = TouchSender {
            stream: UnixStream::connect(Path::new("/nonexistent")).unwrap_err().into(),
            serial: 1,
        };
        let _ = sender.send_touch_down(0, 0.5, 0.5);
    }
}

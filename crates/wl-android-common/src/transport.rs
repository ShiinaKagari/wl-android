use std::io;

use crate::proto::Message;

pub trait Transport {
    fn send(&mut self, msg: &Message) -> io::Result<()>;

    fn recv(&mut self) -> io::Result<Option<Message>>;
}

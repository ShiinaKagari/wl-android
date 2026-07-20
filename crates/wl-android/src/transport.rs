use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use nix::sys::socket::{self, MsgFlags, ControlMessage, ControlMessageOwned};
use wl_android_common::proto::{self, Message};

pub struct Transport {
    stream: UnixStream,
    recv_buf: Vec<u8>,
}

impl Transport {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self { stream, recv_buf: vec![0u8; 65536] })
    }

    pub fn send(&mut self, msg: &Message) -> io::Result<()> {
        let bytes = proto::encode(msg);
        let len_bytes = (bytes.len() as u32).to_le_bytes();

        let fds: Vec<RawFd> = match msg {
            Message::Frame(_, fds) if proto::fd_count(msg) > 0 => {
                fds.iter().map(|fd| fd.as_raw_fd()).collect()
            }
            _ => vec![],
        };

        let iov = [
            std::io::IoSlice::new(&len_bytes),
            std::io::IoSlice::new(&bytes),
        ];

        let cmsgs = if !fds.is_empty() {
            vec![ControlMessage::ScmRights(&fds)]
        } else {
            vec![]
        };

        socket::sendmsg::<()>(
            self.stream.as_raw_fd(),
            &iov,
            &cmsgs,
            MsgFlags::empty(),
            None,
        )?;
        Ok(())
    }

    pub fn recv(&mut self) -> io::Result<Option<Message>> {
        use std::io::IoSliceMut;

        let mut cmsg_space = nix::cmsg_space!([RawFd; 4]);

        let (n_bytes, fds) = {
            let mut iov = [IoSliceMut::new(&mut self.recv_buf)];
            let recv_msg = match socket::recvmsg::<()>(
                self.stream.as_raw_fd(),
                &mut iov,
                Some(&mut cmsg_space),
                MsgFlags::MSG_DONTWAIT,
            ) {
                Ok(msg) => msg,
                Err(nix::errno::Errno::EAGAIN) => return Ok(None),
                Err(e) => return Err(e.into()),
            };

            let n = recv_msg.bytes;

            let fds: Vec<OwnedFd> = match recv_msg.cmsgs() {
                Ok(cmsgs) => cmsgs
                    .flat_map(|cmsg| match cmsg {
                        ControlMessageOwned::ScmRights(fds) => fds,
                        _ => vec![],
                    })
                    .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
                    .collect(),
                Err(_) => vec![],
            };

            (n, fds)
        };

        let data = &self.recv_buf[..n_bytes];
        if data.is_empty() {
            return Ok(None);
        }
        if data.len() < 4 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "message too short"));
        }

        let msg_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        if data.len() < 4 + msg_len {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated message body"));
        }

        let msg_body = &data[4..4 + msg_len];

        let msg = proto::decode(msg_body, fds)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        Ok(Some(msg))
    }
}

impl AsRawFd for Transport {
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use wl_android_common::proto::HelloMessage;

    fn socketpair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("socketpair")
    }

    #[test]
    fn send_recv_hello_roundtrip() {
        let (srv, cli) = socketpair();
        let mut transport = Transport::new(srv).unwrap();
        let mut reader = Transport::new(cli).unwrap();

        let helo = HelloMessage::default();
        transport.send(&Message::Hello(helo)).unwrap();

        // Non-blocking recv: data may arrive asynchronously
        let mut attempts = 0;
        let received = loop {
            match reader.recv().unwrap() {
                Some(msg) => break msg,
                None => {
                    attempts += 1;
                    if attempts > 100 {
                        panic!("recv timed out");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        };
        assert!(matches!(received, Message::Hello(_)));
    }
}

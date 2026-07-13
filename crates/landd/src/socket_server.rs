use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use land_common::protocol::{
    FrameMessage, MessageHeader, MessageType, TouchMessage,
};

use crate::forwarder::{self, Forwarder};

const MAX_CLIENTS: usize = 16;
const EPOLL_EVENTS: usize = 32;
const BUF_SIZE: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientRole {
    Compositor,
    App,
}

struct Client {
    stream: UnixStream,
    role: Option<ClientRole>,
    buf: Vec<u8>,
    buf_pos: usize,
    pending_fds: Vec<RawFd>,
}

pub struct SocketServer {
    listener: UnixListener,
    clients: HashMap<RawFd, Client>,
    forwarder: Forwarder,
    epoll_fd: OwnedFd,
}

impl SocketServer {
    pub fn bind(path: &Path) -> io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;

        // SAFETY: epoll_create1 returns -1 on error (checked below). The returned fd is owned and valid, so OwnedFd::from_raw_fd takes ownership safely.
        let epoll_fd = unsafe {
            let fd = libc::epoll_create1(libc::EPOLL_CLOEXEC);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            OwnedFd::from_raw_fd(fd)
        };

        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLET) as u32,
            u64: listener.as_raw_fd() as u64,
        };
        // SAFETY: epoll_fd and listener fd are valid, and ev is a properly initialized epoll_event.
        unsafe {
            if libc::epoll_ctl(
                epoll_fd.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                listener.as_raw_fd(),
                &mut ev,
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }
        }

        Ok(Self {
            listener,
            clients: HashMap::with_capacity(MAX_CLIENTS),
            forwarder: Forwarder::new(),
            epoll_fd,
        })
    }

    pub fn run(&mut self) -> io::Result<()> {
        let mut events = vec![
            libc::epoll_event { events: 0, u64: 0 };
            EPOLL_EVENTS
        ];

        loop {
            // SAFETY: epoll_fd is a valid epoll fd, events buffer is properly sized, and epoll_wait is safe with no outstanding mutable references.
            let nfds = unsafe {
                libc::epoll_wait(
                    self.epoll_fd.as_raw_fd(),
                    events.as_mut_ptr(),
                    EPOLL_EVENTS as i32,
                    -1,
                )
            };

            if nfds < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }

            for i in 0..nfds as usize {
                let fd = events[i].u64 as RawFd;

                if fd == self.listener.as_raw_fd() {
                    self.accept_client()?;
                } else {
                    self.handle_client(fd)?;
                }
            }
        }
    }

    fn accept_client(&mut self) -> io::Result<()> {
        let (stream, _addr) = self.listener.accept()?;
        stream.set_nonblocking(true)?;
        let fd = stream.as_raw_fd();

        let client = Client {
            stream,
            role: None,
            buf: vec![0u8; BUF_SIZE],
            buf_pos: 0,
            pending_fds: Vec::new(),
        };
        self.clients.insert(fd, client);

        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLRDHUP | libc::EPOLLET) as u32,
            u64: fd as u64,
        };
        // SAFETY: epoll_fd is valid and fd is a newly accepted connected socket; ev is a properly initialized epoll_event.
        unsafe {
            if libc::epoll_ctl(
                self.epoll_fd.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                fd,
                &mut ev,
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }
        }

        eprintln!("[landd] new client fd={}", fd);
        Ok(())
    }

    fn handle_client(&mut self, fd: RawFd) -> io::Result<()> {
        let mut cmsg_buf = [0u8; 4096];

        let client = match self.clients.get_mut(&fd) {
            Some(c) => c,
            None => return Ok(()),
        };

        let mut iov = libc::iovec {
            iov_base: client.buf[client.buf_pos..].as_mut_ptr() as *mut _,
            iov_len: BUF_SIZE - client.buf_pos,
        };

        // SAFETY: zeroed msghdr is safe because all fields are immediately filled in before use.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
        msg.msg_controllen = cmsg_buf.len();

        // SAFETY: recvmsg on a valid connected UnixStream with a properly initialized msghdr.
        let ret = unsafe {
            libc::recvmsg(client.stream.as_raw_fd(), &mut msg, libc::MSG_NOSIGNAL)
        };

        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Ok(());
            }
            if err.kind() == io::ErrorKind::ConnectionReset {
                return self.remove_client(fd);
            }
            return Err(err);
        }

        if ret == 0 {
            return self.remove_client(fd);
        }

        let mut fds = Vec::new();
        // SAFETY: msg.msg_control is set to a valid buffer, so CMSG_FIRSTHDR returns a valid pointer or null.
        let mut cmsg_ptr: *mut libc::cmsghdr = unsafe { libc::CMSG_FIRSTHDR(&msg) };
        while !cmsg_ptr.is_null() {
            // SAFETY: cmsg_ptr is non-null (checked by the loop condition) and points to a valid cmsghdr within the control buffer.
            let cmsg = unsafe { &*cmsg_ptr };
            if cmsg.cmsg_level == libc::SOL_SOCKET && cmsg.cmsg_type == libc::SCM_RIGHTS {
                // SAFETY: CMSG_LEN is a macro computing a constant size; safe to call with 0 payload length.
                let fd_count = (cmsg.cmsg_len as usize - unsafe { libc::CMSG_LEN(0) } as usize)
                    / std::mem::size_of::<RawFd>();
                // SAFETY: CMSG_DATA returns a pointer to the data past the cmsghdr header, valid for the fd payload within the control buffer.
                let fd_data = unsafe { libc::CMSG_DATA(cmsg_ptr) as *const RawFd };
                for i in 0..fd_count {
                    // SAFETY: fd_data.add(i) is within the valid CMSG data region, and read_unaligned handles potential misalignment of RawFd.
                    let received_fd = unsafe { std::ptr::read_unaligned(fd_data.add(i)) };
                    fds.push(received_fd);
                }
            }
            // SAFETY: cmsg_ptr points to a valid cmsghdr within the control buffer; CMSG_NXTHDR advances to the next header or returns null.
            cmsg_ptr = unsafe { libc::CMSG_NXTHDR(&msg, cmsg_ptr) };
        }

        client.pending_fds = fds;
        client.buf_pos += ret as usize;

        if client.buf_pos >= BUF_SIZE {
            client.buf_pos = 0;
            return Err(io::Error::new(io::ErrorKind::InvalidData, "buffer overflow"));
        }

        self.process_client(fd)
    }

    fn process_client(&mut self, fd: RawFd) -> io::Result<()> {
        let header_size = MessageHeader::serialized_size();

        let (header, buf_pos) = {
            let client = self.clients.get(&fd).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "client not found")
            })?;
            if client.buf_pos < header_size {
                return Ok(());
            }
            let mut hb = [0u8; 12];
            hb.copy_from_slice(&client.buf[..header_size]);
            // SAFETY: hb is a correctly-sized byte array on the stack; read_unaligned handles potential misalignment for the MessageHeader type.
            let h: MessageHeader = unsafe { std::ptr::read_unaligned(hb.as_ptr() as *const MessageHeader) };
            (h, client.buf_pos)
        };

        if !header.validate() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid message header"));
        }

        let total_len = header_size + header.length as usize;
        if buf_pos < total_len {
            return Ok(());
        }

        let msg_type = header.msg_type;

        let client_role = if msg_type == MessageType::Frame as u32 {
            ClientRole::Compositor
        } else if msg_type == MessageType::Touch as u32 {
            ClientRole::App
        } else {
            ClientRole::Compositor
        };

        {
            let client = self.clients.get_mut(&fd).unwrap();
            if client.role.is_none() {
                client.role = Some(client_role);
                match client_role {
                    ClientRole::Compositor => self.forwarder.register_compositor(fd),
                    ClientRole::App => self.forwarder.register_app(fd),
                }
            }
        }

        if msg_type == MessageType::Frame as u32 {
            let frame;
            let fds;
            {
                let client = self.clients.get(&fd).unwrap();
                // SAFETY: buf contains at least sizeof(FrameMessage) bytes after header_size (verified by total_len check); read_unaligned handles misalignment.
                frame = unsafe {
                    std::ptr::read_unaligned(client.buf[header_size..].as_ptr() as *const FrameMessage)
                };
                fds = std::mem::take(&mut self.clients.get_mut(&fd).unwrap().pending_fds);
            }

            if let Some(target_fd) = self.forwarder.target_for_frame(fd) {
                if let Some(client) = self.clients.get(&target_fd) {
                    forwarder::send_frame_to(&client.stream, &frame, &fds);
                }
            }
        } else if msg_type == MessageType::Touch as u32 {
            let touch;
            {
                let client = self.clients.get(&fd).unwrap();
                // SAFETY: buf contains at least sizeof(TouchMessage) bytes after header_size (verified by total_len check); read_unaligned handles misalignment.
                touch = unsafe {
                    std::ptr::read_unaligned(client.buf[header_size..].as_ptr() as *const TouchMessage)
                };
            }

            if let Some(target_fd) = self.forwarder.target_for_touch(fd) {
                if let Some(client) = self.clients.get(&target_fd) {
                    forwarder::send_touch_to(&client.stream, &touch);
                }
            }
        }

        {
            let client = self.clients.get_mut(&fd).unwrap();
            let remaining = client.buf_pos - total_len;
            if remaining > 0 {
                client.buf.copy_within(total_len..client.buf_pos, 0);
            }
            client.buf_pos = remaining;
        }

        self.process_client(fd)
    }

    fn remove_client(&mut self, fd: RawFd) -> io::Result<()> {
        let role = self.clients.get(&fd).and_then(|c| c.role);
        if let Some(r) = role {
            match r {
                ClientRole::Compositor => self.forwarder.unregister_compositor(),
                ClientRole::App => self.forwarder.unregister_app(),
            }
        }
        self.clients.remove(&fd);
        // SAFETY: epoll_fd is valid and fd was previously registered via EPOLL_CTL_ADD; EPOLL_CTL_DEL with a null event pointer is valid.
        unsafe {
            libc::epoll_ctl(
                self.epoll_fd.as_raw_fd(),
                libc::EPOLL_CTL_DEL,
                fd,
                std::ptr::null_mut(),
            );
        }
        eprintln!("[landd] client disconnected fd={}", fd);
        Ok(())
    }
}

impl Drop for SocketServer {
    fn drop(&mut self) {
        for fd in self.clients.keys() {
            // SAFETY: epoll_fd is valid and each fd was previously registered; removing on drop is safe even if already removed (kernel ignores unregistered fds).
            unsafe {
                libc::epoll_ctl(
                    self.epoll_fd.as_raw_fd(),
                    libc::EPOLL_CTL_DEL,
                    *fd,
                    std::ptr::null_mut(),
                );
            }
        }
    }
}

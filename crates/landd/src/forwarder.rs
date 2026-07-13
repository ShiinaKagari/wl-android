use std::os::fd::AsRawFd;
use std::os::unix::io::RawFd;

use land_common::protocol::FrameMessage;

/// 转发器：维护 compositor ↔ app 的 fd 映射关系。
///
/// 不持有 stream 引用，由 socket_server 在转发时传入。
pub struct Forwarder {
    compositor_fd: Option<RawFd>,
    app_fd: Option<RawFd>,
}

impl Forwarder {
    pub fn new() -> Self {
        Self { compositor_fd: None, app_fd: None }
    }

    pub fn register_compositor(&mut self, fd: RawFd) {
        self.compositor_fd = Some(fd);
    }

    pub fn register_app(&mut self, fd: RawFd) {
        self.app_fd = Some(fd);
    }

    pub fn unregister_compositor(&mut self) {
        self.compositor_fd = None;
    }

    pub fn unregister_app(&mut self) {
        self.app_fd = None;
    }

    pub fn has_pair(&self) -> bool {
        self.compositor_fd.is_some() && self.app_fd.is_some()
    }

    /// 帧消息的目地 fd：compositor → app
    pub fn target_for_frame(&self, source: RawFd) -> Option<RawFd> {
        if source == self.compositor_fd.unwrap_or(-1) {
            self.app_fd
        } else {
            self.compositor_fd
        }
    }

    /// 触摸消息的目地 fd：app → compositor
    pub fn target_for_touch(&self, source: RawFd) -> Option<RawFd> {
        if source == self.app_fd.unwrap_or(-1) {
            self.compositor_fd
        } else {
            self.app_fd
        }
    }
}

/// 通过 sendmsg + SCM_RIGHTS 发送帧到 stream。
pub fn send_frame_to(
    stream: &std::os::unix::net::UnixStream,
    msg: &FrameMessage,
    fds: &[RawFd],
) {
    let header = land_common::protocol::MessageHeader::new(
        land_common::protocol::MessageType::Frame,
        land_common::protocol::FrameMessage::serialized_size() as u32,
    );

    // SAFETY: header is a valid reference, so the resulting slice covers valid initialized bytes of the correct size.
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            &header as *const _ as *const u8,
            std::mem::size_of::<land_common::protocol::MessageHeader>(),
        )
    };
    // SAFETY: msg is a valid reference to a FrameMessage, producing a slice of valid initialized bytes of the correct size.
    let body_bytes = unsafe {
        std::slice::from_raw_parts(
            msg as *const _ as *const u8,
            std::mem::size_of::<FrameMessage>(),
        )
    };

    let iov = [std::io::IoSlice::new(header_bytes), std::io::IoSlice::new(body_bytes)];

    let fd_count = fds.len();
    // SAFETY: CMSG_SPACE computes the required aligned size for fd_count fds; the result is used only for buffer allocation.
    let cmsg_space = unsafe {
        libc::CMSG_SPACE((fd_count * std::mem::size_of::<RawFd>()) as u32) as usize
    };
    let mut cmsg_buf = vec![0u8; cmsg_space];

    // SAFETY: zeroed msghdr is safe because all fields are immediately filled in before use.
    let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
    msghdr.msg_iov = iov.as_ptr() as *mut _;
    msghdr.msg_iovlen = iov.len();
    msghdr.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
    msghdr.msg_controllen = cmsg_space;

    // SAFETY: msghdr is properly initialized with msg_control pointing to a sufficiently large buffer.
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msghdr) };
    if !cmsg.is_null() {
        // SAFETY: cmsg is valid (non-null), cmsg_buf is large enough for the fd payload (sized via CMSG_SPACE), and copy_nonoverlapping's source/dest regions are valid and non-overlapping.
        unsafe {
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(
                (fd_count * std::mem::size_of::<RawFd>()) as u32,
            ) as _;
            let fd_data = libc::CMSG_DATA(cmsg) as *mut RawFd;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), fd_data, fd_count);
        }
    }

    // SAFETY: stream is a valid connected UnixStream, msghdr is fully initialized, and MSG_NOSIGNAL prevents SIGPIPE.
    let ret = unsafe { libc::sendmsg(stream.as_raw_fd(), &msghdr, libc::MSG_NOSIGNAL) };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        log::error!("[landd] forward_frame sendmsg failed: {}", err);
    }
}

/// 发送触摸消息到 stream。
pub fn send_touch_to(
    stream: &std::os::unix::net::UnixStream,
    msg: &land_common::protocol::TouchMessage,
) {
    let header = land_common::protocol::MessageHeader::new(
        land_common::protocol::MessageType::Touch,
        land_common::protocol::TouchMessage::serialized_size() as u32,
    );

    // SAFETY: header is a valid reference, so the resulting slice covers valid initialized bytes of the correct size.
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            &header as *const _ as *const u8,
            std::mem::size_of::<land_common::protocol::MessageHeader>(),
        )
    };
    // SAFETY: msg is a valid reference to a TouchMessage, producing a slice of valid initialized bytes of the correct size.
    let body_bytes = unsafe {
        std::slice::from_raw_parts(
            msg as *const _ as *const u8,
            std::mem::size_of::<land_common::protocol::TouchMessage>(),
        )
    };

    let iov = [std::io::IoSlice::new(header_bytes), std::io::IoSlice::new(body_bytes)];
    // SAFETY: zeroed msghdr is safe because all fields are immediately filled in before use.
    let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
    msghdr.msg_iov = iov.as_ptr() as *mut _;
    msghdr.msg_iovlen = iov.len();

    // SAFETY: stream is a valid connected UnixStream, msghdr is initialized, and MSG_NOSIGNAL prevents SIGPIPE.
    let ret = unsafe { libc::sendmsg(stream.as_raw_fd(), &msghdr, libc::MSG_NOSIGNAL) };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        log::error!("[landd] forward_touch sendmsg failed: {}", err);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_registration() {
        let mut f = Forwarder::new();
        assert!(!f.has_pair());

        f.register_compositor(3);
        f.register_app(4);
        assert!(f.has_pair());
        assert_eq!(f.target_for_frame(3), Some(4));
        assert_eq!(f.target_for_touch(4), Some(3));
    }

    #[test]
    fn forward_no_target() {
        let f = Forwarder::new();
        assert_eq!(f.target_for_frame(0), None);
        assert_eq!(f.target_for_touch(0), None);
    }
}

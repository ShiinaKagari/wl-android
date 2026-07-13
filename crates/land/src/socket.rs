use std::io::{self, IoSlice};
use std::os::fd::AsRawFd;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

use land_common::protocol::{FrameMessage, MessageHeader, MessageType};

pub struct FrameSender {
    stream: UnixStream,
}

impl FrameSender {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Ok(Self { stream })
    }

    pub fn send_frame(
        &self,
        fds: &[RawFd],
        width: u32,
        height: u32,
        format: u32,
        stride: u32,
        serial: u64,
    ) -> io::Result<()> {
        let frame = FrameMessage::new(width, height, format, stride, serial);
        let header = MessageHeader::new(MessageType::Frame, FrameMessage::serialized_size() as u32);

        // SAFETY: sendmsg_with_fds 要求 header/frame 为有效引用，fds 为有效且已 dup 的 fd，
        // 调用者 (send_frame) 持有这些变量的所有权，保证调用期间有效。
        unsafe { self.sendmsg_with_fds(&header, &frame, fds) }
    }

    /// # SAFETY
    /// - `header` and `body` must be valid references (valid for reads, properly aligned)
    /// - `fds` must contain valid, dup'd file descriptors
    unsafe fn sendmsg_with_fds(
        &self,
        header: &MessageHeader,
        body: &FrameMessage,
        fds: &[RawFd],
    ) -> io::Result<()> {
        // SAFETY: header/body 是 &T，其指针有效且对齐；size_of 保证 slice 不越界
        let header_bytes = unsafe {
            std::slice::from_raw_parts(
                header as *const MessageHeader as *const u8,
                std::mem::size_of::<MessageHeader>(),
            )
        };
        // SAFETY: 同上
        let body_bytes = unsafe {
            std::slice::from_raw_parts(
                body as *const FrameMessage as *const u8,
                std::mem::size_of::<FrameMessage>(),
            )
        };

        let iov = [IoSlice::new(header_bytes), IoSlice::new(body_bytes)];

        let fd_count = fds.len();
        let fd_size = std::mem::size_of::<RawFd>();
        // SAFETY: CMSG_SPACE 是宏，计算结果保证足够容纳 fd
        let cmsg_space = unsafe { libc::CMSG_SPACE((fd_count * fd_size) as u32) as usize };

        let mut cmsg_buf = vec![0u8; cmsg_space];

        // SAFETY: zeroed() 对所有字段赋 0，msghdr 所有位模式均有效
        let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
        msghdr.msg_iov = iov.as_ptr() as *mut libc::iovec;
        msghdr.msg_iovlen = iov.len();
        msghdr.msg_control = cmsg_buf.as_mut_ptr() as *mut std::ffi::c_void;
        msghdr.msg_controllen = cmsg_space;

        // SAFETY: CMSG_FIRSTHDR 在 msghdr 初始化后调用，返回指向 cmsg_buf 的指针
        let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msghdr) };
        if !cmsg.is_null() {
            // SAFETY: cmsg 非空，指向 cmsg_buf 内有效内存；CMSG_DATA 返回 fd 写入位置
            unsafe {
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN((fd_count * fd_size) as u32) as _;

                let fd_data = libc::CMSG_DATA(cmsg) as *mut RawFd;
                std::ptr::copy_nonoverlapping(fds.as_ptr(), fd_data, fd_count);
            }
        }

        // SAFETY: sendmsg 是标准系统调用，msghdr 已在前面正确初始化
        let ret = unsafe { libc::sendmsg(self.stream.as_raw_fd(), &msghdr, libc::MSG_NOSIGNAL) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_no_socket() {
        let result = FrameSender::connect(Path::new("/tmp/nonexistent.sock"));
        assert!(result.is_err());
    }
}

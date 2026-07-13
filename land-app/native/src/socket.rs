use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use land_common::protocol::{FrameMessage, MessageHeader, MessageType};

/// 最新帧缓存
pub static LATEST_FRAME: Mutex<Option<OwnedFd>> = Mutex::new(None);
pub static FRAME_META: Mutex<Option<FrameMessage>> = Mutex::new(None);
pub static CONNECTED: AtomicBool = AtomicBool::new(false);

/// 连接 socketd，接收 frame。
pub fn start() -> std::io::Result<()> {
    let socket_path = land_common::types::default_socket_path();

    std::thread::spawn(move || loop {
        match UnixStream::connect(&socket_path) {
            Ok(stream) => {
                CONNECTED.store(true, Ordering::SeqCst);
                log::info!("[land-app] connected to socketd");
                if let Err(e) = recv_loop(stream) {
                    log::info!("[land-app] disconnected: {}", e);
                }
                CONNECTED.store(false, Ordering::SeqCst);
            }
            Err(e) => {
                log::warn!("[land-app] socketd not ready: {}", e);
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    });

    Ok(())
}

fn recv_loop(stream: UnixStream) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut pos = 0;
    let hsize = MessageHeader::serialized_size();

    loop {
        let mut cmsg = [0u8; 1024];
        let mut iov = libc::iovec {
            iov_base: buf[pos..].as_mut_ptr() as *mut _,
            iov_len: buf.len() - pos,
        };
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg.as_mut_ptr() as *mut _;
        msg.msg_controllen = cmsg.len();

        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_NOSIGNAL) };
        if n <= 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "closed"));
        }

        let mut fds = Vec::new();
        let mut cp = unsafe { libc::CMSG_FIRSTHDR(&msg) };
        while !cp.is_null() {
            let c = unsafe { &*cp };
            if c.cmsg_level == libc::SOL_SOCKET && c.cmsg_type == libc::SCM_RIGHTS {
                let count = (c.cmsg_len as usize - unsafe { libc::CMSG_LEN(0) } as usize)
                    / std::mem::size_of::<RawFd>();
                let data = unsafe { libc::CMSG_DATA(cp) as *const RawFd };
                for i in 0..count {
                    fds.push(unsafe { std::ptr::read(data.add(i)) });
                }
            }
            cp = unsafe { libc::CMSG_NXTHDR(&msg, cp) };
        }

        pos += n as usize;
        while pos >= hsize {
            let mut hb = [0u8; 12];
            hb.copy_from_slice(&buf[..hsize]);
            let h: MessageHeader = unsafe { std::ptr::read_unaligned(hb.as_ptr() as *const MessageHeader) };
            if !h.validate() {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad header"));
            }
            let total = hsize + h.length as usize;
            if pos < total { break; }

            if h.msg_type == MessageType::Frame as u32 && !fds.is_empty() {
                let frame: FrameMessage = unsafe {
                    std::ptr::read_unaligned(buf[hsize..].as_ptr() as *const FrameMessage)
                };
                if let Ok(mut g) = FRAME_META.lock() { *g = Some(frame); }
                if let Ok(mut g) = LATEST_FRAME.lock() {
                    *g = unsafe { Some(OwnedFd::from_raw_fd(fds[0])) };
                }
            } else {
                for &fd in &fds { unsafe { libc::close(fd); } }
            }
            fds.clear();
            buf.copy_within(total..pos, 0);
            pos -= total;
        }
    }
}

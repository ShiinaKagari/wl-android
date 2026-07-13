use std::io::IoSlice;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use land_common::protocol::{FrameMessage, MessageHeader, MessageType};

/// 最新帧缓存，由 server 线程写入，渲染线程读取。
pub static LATEST_FRAME: Mutex<Option<OwnedFd>> = Mutex::new(None);
pub static FRAME_META: Mutex<Option<FrameMessage>> = Mutex::new(None);
pub static CONNECTED: AtomicBool = AtomicBool::new(false);

/// 启动 socket server 线程。
/// 监听 `/data/local/tmp/land.sock` 或 `LAND_SOCKET` 环境变量。
/// 每个 compositor 连接进来后循环接收 FrameMessage + SCM_RIGHTS fd，
/// 写入最新帧缓存。连接断开后等待下一个。
pub fn start_server() -> std::io::Result<()> {
    let socket_path = land_common::types::default_socket_path();

    // 删除残留 socket 文件
    let _ = std::fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path)?;
    // 0666 权限
    #[cfg(target_os = "android")]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o666)).ok();
    }

    log::info!("[land-server] listening on {}", socket_path.display());

    std::thread::spawn(move || {
        // 每次 accept 一个 compositor 连接，断开后等待下一个
        loop {
            let stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(e) => {
                    log::error!("[land-server] accept failed: {}", e);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            };

            stream.set_nonblocking(true).ok();
            let fd = stream.as_raw_fd();
            CONNECTED.store(true, Ordering::SeqCst);
            log::info!("[land-server] compositor connected fd={}", fd);

            if let Err(e) = handle_compositor(stream) {
                log::info!("[land-server] compositor disconnected: {}", e);
            }

            CONNECTED.store(false, Ordering::SeqCst);
        }
    });

    Ok(())
}

fn handle_compositor(stream: UnixStream) -> std::io::Result<()> {
    let mut buf = vec![0u8; 4 * 1024 * 1024]; // 4MB recv buffer
    let mut buf_pos = 0;
    let header_size = MessageHeader::serialized_size();

    loop {
        // 接收数据 + SCM_RIGHTS fd
        let mut iov = libc::iovec {
            iov_base: buf[buf_pos..].as_mut_ptr() as *mut _,
            iov_len: buf.len() - buf_pos,
        };
        let mut cmsg_buf = [0u8; 1024];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
        msg.msg_controllen = cmsg_buf.len();

        let ret = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_NOSIGNAL) };
        if ret <= 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "disconnected"));
        }

        // 提取 SCM_RIGHTS fd
        let mut fds = Vec::new();
        let mut cmsg_ptr = unsafe { libc::CMSG_FIRSTHDR(&msg) };
        while !cmsg_ptr.is_null() {
            let cmsg = unsafe { &*cmsg_ptr };
            if cmsg.cmsg_level == libc::SOL_SOCKET && cmsg.cmsg_type == libc::SCM_RIGHTS {
                let count = (cmsg.cmsg_len as usize - unsafe { libc::CMSG_LEN(0) } as usize)
                    / std::mem::size_of::<RawFd>();
                let data = unsafe { libc::CMSG_DATA(cmsg_ptr) as *const RawFd };
                for i in 0..count {
                    fds.push(unsafe { std::ptr::read(data.add(i)) });
                }
            }
            cmsg_ptr = unsafe { libc::CMSG_NXTHDR(&msg, cmsg_ptr) };
        }

        buf_pos += ret as usize;
        debug_assert!(buf_pos <= buf.len());

        // 解析消息
        while buf_pos >= header_size {
            let mut hb = [0u8; 12];
            hb.copy_from_slice(&buf[..header_size]);
            let header: MessageHeader = unsafe { std::ptr::read_unaligned(hb.as_ptr() as *const MessageHeader) };
            if !header.validate() {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad header"));
            }

            let total = header_size + header.length as usize;
            if buf_pos < total {
                break;
            }

            if header.msg_type == MessageType::Frame as u32 && !fds.is_empty() {
                let frame: FrameMessage = unsafe {
                    std::ptr::read_unaligned(buf[header_size..].as_ptr() as *const FrameMessage)
                };

                // 写入最新帧缓存
                if let Ok(mut guard) = FRAME_META.lock() {
                    *guard = Some(frame);
                }
                if let Ok(mut guard) = LATEST_FRAME.lock() {
                    *guard = unsafe { Some(OwnedFd::from_raw_fd(fds[0])) };
                }
            } else {
                // 关掉未使用的 fd
                for fd in &fds {
                    unsafe { libc::close(*fd); }
                }
            }

            fds.clear();
            buf.copy_within(total..buf_pos, 0);
            buf_pos -= total;
        }
    }
}

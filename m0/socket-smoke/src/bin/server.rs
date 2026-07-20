use nix::sys::memfd::{memfd_create, MemFdCreateFlag};
use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags};
use nix::unistd::{lseek, read, write, Whence};
use std::ffi::CString;
use std::io::IoSliceMut;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixListener;

fn main() {
    let socket_path = std::env::var("SMOKE_SOCKET").unwrap_or_else(|_| "/tmp/m0-smoke.sock".to_string());
    std::fs::remove_file(&socket_path).ok();

    let listener = UnixListener::bind(&socket_path).expect("bind");
    println!("[server] Listening on {socket_path}");

    let (conn, _) = listener.accept().expect("accept");
    println!("[server] Client connected");

    // Create a test memfd to send
    let test_data = b"Hello from server via SCM_RIGHTS!";
    let name = CString::new("smoke-test").unwrap();
    let fd = memfd_create(&name, MemFdCreateFlag::MFD_ALLOW_SEALING).expect("memfd_create");
    write(&fd, test_data).expect("write");

    let fd_raw = fd.as_raw_fd();
    let iov_out = [std::io::IoSlice::new(b"hello from server")];
    let cmsgs_out = [ControlMessage::ScmRights(&[fd_raw])];
    sendmsg::<()>(conn.as_raw_fd(), &iov_out, &cmsgs_out, MsgFlags::empty(), None)
        .expect("sendmsg with SCM_RIGHTS");
    println!("[server] Sent memfd via SCM_RIGHTS");

    // Receive response — two-phase to avoid borrow issues
    let mut msg_buf = vec![0u8; 256];
    let mut cmsg_buf = nix::cmsg_space!([RawFd; 4]);
    let n_bytes;
    let cmsg_result;
    {
        let mut iov = [IoSliceMut::new(&mut msg_buf)];
        let msg = recvmsg::<()>(
            conn.as_raw_fd(),
            &mut iov,
            Some(&mut cmsg_buf),
            MsgFlags::empty(),
        )
        .expect("recvmsg");
        n_bytes = msg.bytes;
        cmsg_result = msg.cmsgs().map(|c| c.collect::<Vec<_>>());
    }

    let text = String::from_utf8_lossy(&msg_buf[..n_bytes]);
    println!("[server] Client says: {text}");

    let mut fds_back = 0usize;
    if let Ok(cmsgs) = cmsg_result {
        for cmsg in cmsgs {
            if let ControlMessageOwned::ScmRights(fds) = cmsg {
                fds_back = fds.len();
                for fd_val in fds {
                    lseek(fd_val, 0, Whence::SeekSet).ok();
                    let mut rd = [0u8; 128];
                    if let Ok(n) = read(fd_val, &mut rd) {
                        println!(
                            "[server] Read from rx fd: {}",
                            String::from_utf8_lossy(&rd[..n])
                        );
                    }
                }
            }
        }
    }

    println!(
        "[server] {} SCM_RIGHTS roundtrip",
        if fds_back > 0 { "✅" } else { "❌" }
    );

    std::fs::remove_file(&socket_path).ok();
}

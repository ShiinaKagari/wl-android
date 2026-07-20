use nix::sys::memfd::{memfd_create, MemFdCreateFlag};
use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags};
use nix::unistd::{lseek, read, write, Whence};
use std::ffi::CString;
use std::io::IoSliceMut;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

fn main() {
    let socket_path = std::env::var("SMOKE_SOCKET").unwrap_or_else(|_| "/tmp/m0-smoke.sock".to_string());

    let conn = loop {
        match UnixStream::connect(&socket_path) {
            Ok(c) => break c,
            Err(e) => {
                eprintln!("[client] Waiting for server... ({e})");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    };

    println!("[client] Connected to {socket_path}");

    // Receive memfd from server
    let mut msg_buf = vec![0u8; 256];
    let mut cmsg_buf = nix::cmsg_space!([RawFd; 4]);
    let cmsg_result;
    {
        let mut iov = [IoSliceMut::new(&mut msg_buf)];
        let msg = recvmsg::<()>(conn.as_raw_fd(), &mut iov, Some(&mut cmsg_buf), MsgFlags::empty())
            .expect("recvmsg");
        let _n = msg.bytes;
        cmsg_result = msg.cmsgs().map(|c| c.collect::<Vec<_>>());
    }

    let mut received_fd: Option<RawFd> = None;
    if let Ok(cmsgs) = cmsg_result {
        for cmsg in cmsgs {
            if let ControlMessageOwned::ScmRights(mut fds) = cmsg {
                if !fds.is_empty() {
                    received_fd = Some(fds.remove(0));
                    println!("[client] Received fd via SCM_RIGHTS");
                }
            }
        }
    }

    let rx = received_fd.expect("No fd received");
    lseek(rx, 0, Whence::SeekSet).expect("lseek");
    let mut rd = [0u8; 128];
    let n = read(rx, &mut rd).expect("read");
    println!(
        "[client] Server memfd says: {}",
        String::from_utf8_lossy(&rd[..n])
    );

    // Send response with fd back
    let name = CString::new("smoke-response").unwrap();
    let resp_fd = memfd_create(&name, MemFdCreateFlag::MFD_ALLOW_SEALING).expect("memfd_create");
    write(&resp_fd, b"pong from client!").expect("write");

    let resp_raw = resp_fd.as_raw_fd();
    let iov = [std::io::IoSlice::new(b"ACK: got your message")];
    let cmsgs = [ControlMessage::ScmRights(&[resp_raw])];
    sendmsg::<()>(conn.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
        .expect("sendmsg with SCM_RIGHTS");

    println!("[client] Sent response + fd back to server");
    println!("[client] ✅ SCM_RIGHTS roundtrip SUCCESS");
}

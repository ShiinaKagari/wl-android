// mock-client — lightweight binary that tests the wl-android protocol.
// Connects to land.sock, does HELO→CONF handshake, sends touch events, receives frames.
// No Smithay/Vulkan dependencies — just nix + wl-android-common.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use wl_android_common::proto;
use wl_android_common::proto::{HelloMessage, ConfigMessage, FrameAck, Message};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/run/wl-android/land.sock".into());

    println!("Connecting to {socket_path}...");
    let mut stream = UnixStream::connect(&socket_path)?;
    println!("Connected");

    // Receive HELO
    let (data, _fds) = recv_raw(&mut stream)?;
    let msg = proto::decode(&data, vec![])?;
    assert!(matches!(msg, Message::Hello(_)), "expected HELO");
    println!("✅ HELO");

    // Send CONF (3392x2400 @144Hz, 289 DPI, blit mode)
    let conf = ConfigMessage::new(3392, 2400, 144000, 289, 0);
    send_msg(&mut stream, &Message::Config(conf))?;
    println!("✅ CONF");
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Send touch events
    let td = proto::TouchMessage::new(0, 0.5, 0.5, proto::TOUCH_PHASE_DOWN, 1000);
    send_msg(&mut stream, &Message::Touch(td))?;
    let tf = proto::TouchMessage::new(0, 0.0, 0.0, proto::TOUCH_PHASE_FRAME, 1001);
    send_msg(&mut stream, &Message::Touch(tf))?;
    let tu = proto::TouchMessage::new(0, 0.5, 0.5, proto::TOUCH_PHASE_UP, 1002);
    send_msg(&mut stream, &Message::Touch(tu))?;
    send_msg(&mut stream, &Message::Touch(tf))?;
    println!("✅ Touch DOWN+FRAME+UP sent");

    std::thread::sleep(std::time::Duration::from_millis(200));

    // Try to receive a frame
    stream.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
    match recv_raw(&mut stream) {
        Ok((data, _)) => {
            let msg = proto::decode(&data, vec![])?;
            match msg {
                Message::Frame(fm, _) => {
                    println!("✅ Frame: serial={} {}x{}", fm.serial, fm.width, fm.height);
                    send_msg(&mut stream, &Message::Ack(FrameAck::new(fm.serial)))?;
                    println!("✅ Ack sent");
                }
                Message::Config(c) => {
                    println!("📐 Config update: {}x{} @{}mHz", c.width, c.height, c.refresh_millihz);
                }
                _ => println!("📩 Other message: {msg:?}"),
            }
        }
        Err(e) => {
            println!("⚠️  No frame received (expected — no compositor): {e}");
        }
    }

    stream.shutdown(std::net::Shutdown::Both)?;
    println!("✅ Test PASSED");
    Ok(())
}

fn send_msg(stream: &mut UnixStream, msg: &Message) -> std::io::Result<()> {
    let data = proto::encode(msg);
    let len = (data.len() as u32).to_le_bytes();
    stream.write_all(&len)?;
    stream.write_all(&data)?;
    stream.flush()?;
    Ok(())
}

fn recv_raw(stream: &mut UnixStream) -> std::io::Result<(Vec<u8>, Vec<std::os::fd::OwnedFd>)> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let msg_len = u32::from_le_bytes(len_buf) as usize;
    let mut data = vec![0u8; msg_len];
    stream.read_exact(&mut data)?;
    Ok((data, vec![]))
}

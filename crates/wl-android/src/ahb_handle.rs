/// AHB Handle Parser (GZ-001 — sole non-public-contract dependency)
///
/// Parses the wire format sent by AHardwareBuffer_sendHandleToUnixSocket.
/// The Android app uses this public NDK API to transmit AHardwareBuffer's
/// underlying dmabuf fds over the land.sock. The container side does NOT have
/// libandroid, so we parse the wire format manually.
///
/// WIRE FORMAT (verified against AOSP — this is NOT a raw native_handle_t!):
///
/// `AHardwareBuffer_sendHandleToUnixSocket` (frameworks/native/libs/nativewindow/
/// AHardwareBuffer.cpp) does NOT send the libcutils native_handle layout. It
/// flattens the buffer via `GraphicBuffer::flatten()` (frameworks/native/libs/
/// ui/GraphicBuffer.cpp) and sends THAT byte blob over the socket with the fds
/// as SCM_RIGHTS ancillary data:
///
///   struct flat_header {           // 32 bytes, 8 x int32 (little-endian)
///       int32 magic;               // 'GBFR' (0x52464247 LE)
///       int32 width;
///       int32 height;
///       int32 format;              // PixelFormat
///       int32 layerCount;
///       int32 usageLo;             // (uint64 usage) & 0xffffffff
///       int32 usageHi;             // (uint64 usage) >> 32
///       int32 stride;              // pixels
///   };
///   // NO ints follow the header. The dmabuf fd(s) arrive via SCM_RIGHTS.
///
/// There is NO fd count in the header — the number of fds is exactly what the
/// cmsg delivered. A one-image AHB carries one fd (the dma-buf).
///
/// (The libcutils native_handle layout [version][numFds][numInts][ints...] is
/// still parsed as a fallback for robustness/tests.)
use std::os::fd::OwnedFd;

/// `'GBFR'` little-endian — GraphicBuffer::flatten magic (AOSP).
pub const FLAT_MAGIC: u32 = 0x5246_4247; // = u32::from_le_bytes(*b"GBFR")
/// Flat header is 8 x i32 = 32 bytes.
const FLAT_HEADER_LEN: usize = 32;

#[allow(dead_code)]
#[derive(Debug)]
pub struct ParsedHandle {
    pub version: i32,
    pub num_fds: i32,
    pub num_ints: i32,
    pub ints: Vec<i32>,
    pub fds: Vec<OwnedFd>,
}

/// Parse the handle wire data received from AHardwareBuffer_sendHandleToUnixSocket.
/// Returns None if the data is too short or malformed.
#[allow(dead_code)]
pub fn parse_native_handle(data: &[u8], fds: Vec<OwnedFd>) -> Option<ParsedHandle> {
    if data.len() < 4 {
        return None; // need at least the magic to decide the format
    }
    let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    if magic == FLAT_MAGIC {
        parse_flat_handle(data, fds)
    } else {
        parse_legacy_native_handle(data, fds)
    }
}

/// GraphicBuffer::flatten format: 32-byte fixed header + SCM_RIGHTS fds.
/// The fd count is whatever the cmsg delivered (no count in the header).
fn parse_flat_handle(data: &[u8], fds: Vec<OwnedFd>) -> Option<ParsedHandle> {
    if data.len() < FLAT_HEADER_LEN {
        return None; // truncated header
    }
    let num_fds = fds.len() as i32;
    if num_fds < 1 {
        return None; // a buffer handle without any fd is unusable
    }
    Some(ParsedHandle {
        version: 0, // flat format has no version field
        num_fds,
        num_ints: 0,
        ints: Vec::new(),
        fds,
    })
}

/// Legacy libcutils native_handle layout: [version][numFds][numInts][ints...].
fn parse_legacy_native_handle(data: &[u8], fds: Vec<OwnedFd>) -> Option<ParsedHandle> {
    if data.len() < 12 {
        return None; // need at least 3 i32 header fields
    }

    let version = i32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let num_fds = i32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let num_ints = i32::from_le_bytes([data[8], data[9], data[10], data[11]]);

    if num_fds < 0 || num_ints < 0 {
        return None;
    }
    if fds.len() < num_fds as usize {
        return None; // not enough fds received
    }
    let expected_data_len = 12 + (num_ints as usize) * 4;
    if data.len() < expected_data_len {
        return None;
    }

    let mut ints = Vec::with_capacity(num_ints as usize);
    for i in 0..num_ints as usize {
        let offset = 12 + i * 4;
        let val = i32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        ints.push(val);
    }

    // Take ownership of exactly num_fds fds
    let mut fds = fds;
    let handle_fds: Vec<OwnedFd> = fds.drain(..num_fds as usize).collect();

    Some(ParsedHandle {
        version,
        num_fds,
        num_ints,
        ints,
        fds: handle_fds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    fn make_test_fd() -> OwnedFd {
        let (a, _b) = UnixStream::pair().unwrap();
        a.into()
    }

    /// GraphicBuffer::flatten wire bytes: [GBFR][w][h][fmt][layers][usageLo][usageHi][stride].
    fn flat_bytes(w: i32, h: i32) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&0x5246_4247u32.to_le_bytes()); // 'GBFR'
        data.extend_from_slice(&w.to_le_bytes());
        data.extend_from_slice(&h.to_le_bytes());
        data.extend_from_slice(&1i32.to_le_bytes()); // format RGBA_8888
        data.extend_from_slice(&1i32.to_le_bytes()); // layerCount
        data.extend_from_slice(&0x100u32.to_le_bytes()); // usageLo GPU_SAMPLED
        data.extend_from_slice(&0i32.to_le_bytes()); // usageHi
        data.extend_from_slice(&(w * 4).to_le_bytes()); // stride (pixels)
        data
    }

    #[test]
    fn parse_flat_handle_real_aosp_format() {
        let fd = make_test_fd();
        let data = flat_bytes(3392, 2400);
        assert_eq!(data.len(), 32);

        let result = parse_native_handle(&data, vec![fd]).unwrap();
        assert_eq!(result.num_fds, 1);
        assert_eq!(result.num_ints, 0);
        assert!(result.ints.is_empty());
        assert_eq!(result.fds.len(), 1);
    }

    #[test]
    fn parse_flat_handle_truncated_header() {
        let fd = make_test_fd();
        let mut data = flat_bytes(100, 100);
        data.truncate(20); // cut inside the 32-byte header
        assert!(parse_native_handle(&data, vec![fd]).is_none());
    }

    #[test]
    fn parse_flat_handle_no_fd_rejected() {
        let data = flat_bytes(100, 100);
        assert!(parse_native_handle(&data, vec![]).is_none());
    }

    #[test]
    fn parse_valid_legacy_handle() {
        let fd = make_test_fd();
        // native_handle: version=placeholder, numFds=1, numInts=2, ints=[42, 99]
        let mut data = Vec::new();
        data.extend_from_slice(&(-1i32).to_le_bytes()); // version placeholder
        data.extend_from_slice(&1i32.to_le_bytes());     // numFds
        data.extend_from_slice(&2i32.to_le_bytes());     // numInts
        data.extend_from_slice(&42i32.to_le_bytes());
        data.extend_from_slice(&99i32.to_le_bytes());

        let result = parse_native_handle(&data, vec![fd]).unwrap();
        assert_eq!(result.num_fds, 1);
        assert_eq!(result.num_ints, 2);
        assert_eq!(result.ints, vec![42, 99]);
        assert_eq!(result.fds.len(), 1);
    }

    #[test]
    fn parse_too_short() {
        let data = [0u8; 4];
        assert!(parse_native_handle(&data, vec![]).is_none());
    }

    #[test]
    fn parse_legacy_not_enough_fds() {
        let mut data = Vec::new();
        data.extend_from_slice(&0i32.to_le_bytes());
        data.extend_from_slice(&2i32.to_le_bytes()); // numFds=2
        data.extend_from_slice(&0i32.to_le_bytes());
        let fd = make_test_fd();
        assert!(parse_native_handle(&data, vec![fd]).is_none());
    }
}

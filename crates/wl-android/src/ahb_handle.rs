/// AHB Handle Parser (GZ-001 — sole non-public-contract dependency)
///
/// Parses the wire format sent by AHardwareBuffer_sendHandleToUnixSocket.
/// The Android app uses this public NDK API to transmit AHardwareBuffer's
/// underlying dmabuf fds over the land.sock. The container side does NOT have
/// libandroid, so we parse the wire format manually.
///
/// WIRE FORMAT (verified against AOSP GraphicBuffer::flatten — this is NOT a
/// raw native_handle_t!):
///
/// `AHardwareBuffer_sendHandleToUnixSocket` (frameworks/native/libs/nativewindow/
/// AHardwareBuffer.cpp) flattens the buffer via `GraphicBuffer::flatten()`
/// (frameworks/native/libs/ui/GraphicBuffer.cpp) and sends THAT byte blob over
/// the socket with the fds as SCM_RIGHTS ancillary data. The flatten layout
/// (main branch — Android 15/16):
///
///   buf[0]  = 'GB01'   // NEW magic (LE bytes on the wire: 31 30 42 47)
///   buf[1]  = width
///   buf[2]  = height
///   buf[3]  = stride
///   buf[4]  = format
///   buf[5]  = layerCount
///   buf[6]  = int(usage)          // low 32 bits
///   buf[7]  = int(mId >> 32)
///   buf[8]  = int(mId & 0xFFFFFFFF)
///   buf[9]  = int(mGenerationNumber)
///   buf[10] = int(transportNumFds)  // ← fd count lives here!
///   buf[11] = int(transportNumInts)
///   buf[12] = int(usage >> 32)      // high 32 bits
///   // transportNumInts ints follow; the fds arrive via SCM_RIGHTS.
///
/// flattenedSize = (13 + transportNumInts) * 4. The legacy 'GBFR' magic
/// (12 ints, 32-bit usage) is kept as a fallback for older Android.
use std::os::fd::OwnedFd;

/// `'GB01'` — GraphicBuffer::flatten magic, LE bytes on the wire (Android 15+).
pub const FLAT_MAGIC: u32 = 0x3130_4247; // bytes on wire: 31 30 42 47 ("10BG" LE = 'GB01')
/// Legacy `'GBFR'` magic (pre-Android-15 flatten, 32-bit usage).
pub const FLAT_MAGIC_LEGACY: u32 = 0x5246_4247; // wire bytes: 47 42 46 52
/// New flatten header: 13 x i32 = 52 bytes.
const FLAT_HEADER_LEN: usize = 52;
/// Legacy flatten header: 12 x i32 = 48 bytes.
const FLAT_HEADER_LEN_LEGACY: usize = 48;
/// Offset of transportNumFds in the new (13-int) header.
const FLAT_NUM_FDS_OFFSET: usize = 10 * 4;
/// Offset of transportNumFds in the legacy (12-int) header.
const FLAT_NUM_FDS_OFFSET_LEGACY: usize = 10 * 4;

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
    if magic == FLAT_MAGIC || magic == FLAT_MAGIC_LEGACY {
        parse_flat_handle(data, fds, magic == FLAT_MAGIC)
    } else {
        parse_legacy_native_handle(data, fds)
    }
}

/// GraphicBuffer::flatten format: fixed header + SCM_RIGHTS fds.
/// `new_format` selects the 13-int ('GB01') vs 12-int ('GBFR') header; the fd
/// count is read from header word 10 (transportNumFds) in both.
fn parse_flat_handle(data: &[u8], fds: Vec<OwnedFd>, new_format: bool) -> Option<ParsedHandle> {
    let header_len = if new_format { FLAT_HEADER_LEN } else { FLAT_HEADER_LEN_LEGACY };
    let num_fds_offset = if new_format { FLAT_NUM_FDS_OFFSET } else { FLAT_NUM_FDS_OFFSET_LEGACY };
    if data.len() < header_len {
        return None; // truncated header
    }
    let num_fds = i32::from_le_bytes([
        data[num_fds_offset],
        data[num_fds_offset + 1],
        data[num_fds_offset + 2],
        data[num_fds_offset + 3],
    ]);
    let num_ints = if new_format {
        i32::from_le_bytes([
            data[11 * 4],
            data[11 * 4 + 1],
            data[11 * 4 + 2],
            data[11 * 4 + 3],
        ])
    } else {
        0
    };
    if num_fds < 0 || num_ints < 0 {
        return None;
    }
    if fds.len() < num_fds as usize {
        return None; // not enough fds received
    }
    if data.len() < header_len + (num_ints as usize) * 4 {
        return None;
    }

    let mut ints = Vec::with_capacity(num_ints as usize);
    for i in 0..num_ints as usize {
        let offset = header_len + i * 4;
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
        version: 0, // flat format has no version field
        num_fds,
        num_ints,
        ints,
        fds: handle_fds,
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

    /// GraphicBuffer::flatten wire bytes (Android 15+ 'GB01', 13 ints).
    fn flat_bytes(w: i32, h: i32) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&0x3130_4247u32.to_le_bytes()); // 'GB01' LE
        data.extend_from_slice(&w.to_le_bytes()); // width
        data.extend_from_slice(&h.to_le_bytes()); // height
        data.extend_from_slice(&(w * 4).to_le_bytes()); // stride
        data.extend_from_slice(&1i32.to_le_bytes()); // format RGBA_8888
        data.extend_from_slice(&1i32.to_le_bytes()); // layerCount
        data.extend_from_slice(&0x100u32.to_le_bytes()); // usage low
        data.extend_from_slice(&0i32.to_le_bytes()); // mId high
        data.extend_from_slice(&0i32.to_le_bytes()); // mId low
        data.extend_from_slice(&0i32.to_le_bytes()); // generation
        data.extend_from_slice(&1i32.to_le_bytes()); // transportNumFds
        data.extend_from_slice(&0i32.to_le_bytes()); // transportNumInts
        data.extend_from_slice(&0i32.to_le_bytes()); // usage high
        data
    }

    #[test]
    fn parse_flat_handle_real_aosp_format() {
        let fd = make_test_fd();
        let data = flat_bytes(3392, 2400);
        assert_eq!(data.len(), 52);

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
        data.truncate(40); // cut inside the 52-byte header
        assert!(parse_native_handle(&data, vec![fd]).is_none());
    }

    #[test]
    fn parse_flat_handle_no_fd_rejected() {
        let data = flat_bytes(100, 100);
        assert!(parse_native_handle(&data, vec![]).is_none());
    }

    #[test]
    fn parse_flat_handle_legacy_gbfr_format() {
        let fd = make_test_fd();
        let mut data = Vec::new();
        data.extend_from_slice(&0x5246_4247u32.to_le_bytes()); // 'GBFR' LE
        data.extend_from_slice(&100i32.to_le_bytes()); // width
        data.extend_from_slice(&100i32.to_le_bytes()); // height
        data.extend_from_slice(&400i32.to_le_bytes()); // stride
        data.extend_from_slice(&1i32.to_le_bytes()); // format
        data.extend_from_slice(&1i32.to_le_bytes()); // layerCount
        data.extend_from_slice(&0x100u32.to_le_bytes()); // usage
        data.extend_from_slice(&0i32.to_le_bytes()); // mId high
        data.extend_from_slice(&0i32.to_le_bytes()); // mId low
        data.extend_from_slice(&0i32.to_le_bytes()); // generation
        data.extend_from_slice(&1i32.to_le_bytes()); // transportNumFds
        data.extend_from_slice(&0i32.to_le_bytes()); // transportNumInts

        let result = parse_native_handle(&data, vec![fd]).unwrap();
        assert_eq!(result.num_fds, 1);
        assert_eq!(result.num_ints, 0);
        assert_eq!(result.fds.len(), 1);
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

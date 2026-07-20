/// AHB Handle Parser (GZ-001 — sole non-public-contract dependency)
///
/// Parses the `native_handle_t` wire format sent by AHardwareBuffer_sendHandleToUnixSocket.
/// The Android app uses this public NDK API to transmit AHardwareBuffer's underlying
/// dmabuf fds over the land.sock. The container side does NOT have libandroid,
/// so we parse the wire format manually.
///
/// Wire format (libcutils native_handle_t):
///   struct native_handle {
///       int version;        // sizeof(native_handle_t)
///       int numFds;         // number of file descriptors
///       int numInts;        // number of ints
///       int data[numFds + numInts]; // fds then ints
///   };
///
/// The fds are transmitted via SCM_RIGHTS alongside the message.
/// The ints follow the header in the data payload.
use std::os::fd::OwnedFd;

#[allow(dead_code)]
#[derive(Debug)]
pub struct ParsedHandle {
    pub version: i32,
    pub num_fds: i32,
    pub num_ints: i32,
    pub ints: Vec<i32>,
    pub fds: Vec<OwnedFd>,
}

/// Parse a native_handle from raw bytes + fds received via SCM_RIGHTS.
/// Returns None if the data is too short or malformed.
#[allow(dead_code)]
pub fn parse_native_handle(data: &[u8], fds: Vec<OwnedFd>) -> Option<ParsedHandle> {
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

    #[test]
    fn parse_valid_handle() {
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
    fn parse_not_enough_fds() {
        let mut data = Vec::new();
        data.extend_from_slice(&0i32.to_le_bytes());
        data.extend_from_slice(&2i32.to_le_bytes()); // numFds=2
        data.extend_from_slice(&0i32.to_le_bytes());
        let fd = make_test_fd();
        assert!(parse_native_handle(&data, vec![fd]).is_none());
    }
}

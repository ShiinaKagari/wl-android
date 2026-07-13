use std::fmt;

/// DRM 四平面格式，对应 DRM_FORMAT_* 常量。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum DrmFormat {
    Xrgb8888 = 0x34325258,
    Argb8888 = 0x34325241,
    Xbgr8888 = 0x34324258,
    Abgr8888 = 0x34324241,
}

impl DrmFormat {
    pub fn from_fourcc(code: u32) -> Option<Self> {
        match code {
            0x34325258 => Some(Self::Xrgb8888),
            0x34325241 => Some(Self::Argb8888),
            0x34324258 => Some(Self::Xbgr8888),
            0x34324241 => Some(Self::Abgr8888),
            _ => None,
        }
    }

    pub fn bpp(&self) -> u32 {
        match self {
            Self::Xrgb8888 | Self::Argb8888 | Self::Xbgr8888 | Self::Abgr8888 => 4,
        }
    }
}

impl fmt::Display for DrmFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Xrgb8888 => "XRGB8888",
            Self::Argb8888 => "ARGB8888",
            Self::Xbgr8888 => "XBGR8888",
            Self::Abgr8888 => "ABGR8888",
        };
        f.write_str(s)
    }
}

/// Unix Socket 路径。
///
/// 默认 `/dev/socket/land.sock`，可通过 `LAND_SOCKET` 环境变量覆盖。
pub fn default_socket_path() -> std::path::PathBuf {
    std::env::var("LAND_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/dev/socket/land.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_format_roundtrip() {
        for code in [0x34325258, 0x34325241, 0x34324258, 0x34324241] {
            let f = DrmFormat::from_fourcc(code).unwrap();
            assert_eq!(f as u32, code);
        }
    }

    #[test]
    fn invalid_fourcc_returns_none() {
        assert!(DrmFormat::from_fourcc(0x12345678).is_none());
    }

    #[test]
    fn default_socket_from_env() {
        unsafe { std::env::set_var("LAND_SOCKET", "/tmp/test.sock") };
        assert_eq!(default_socket_path(), std::path::PathBuf::from("/tmp/test.sock"));
        unsafe { std::env::remove_var("LAND_SOCKET") };
    }

    #[test]
    fn default_socket_fallback() {
        unsafe { std::env::remove_var("LAND_SOCKET") };
        assert_eq!(default_socket_path(), std::path::PathBuf::from("/dev/socket/land.sock"));
    }
}

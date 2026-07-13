use std::ffi::c_int;
use std::os::unix::io::RawFd;

pub const WLR_DMABUF_MAX_PLANES: usize = 4;

/// wlr_dmabuf_attributes 的精确内存布局匹配。
#[derive(Debug, Clone)]
#[repr(C)]
pub struct wlr_dmabuf_attributes {
    pub width: i32,
    pub height: i32,
    pub format: u32,
    pub n_planes: u32,
    pub fd: [c_int; WLR_DMABUF_MAX_PLANES],
    pub offset: [u32; WLR_DMABUF_MAX_PLANES],
    pub stride: [u32; WLR_DMABUF_MAX_PLANES],
    pub modifier: u64,
}

// SAFETY: wlr_buffer_get_dmabuf is a C function; only callable with valid wlr_buffer pointer
unsafe extern "C" {
    /// 从 wlr_buffer 获取 DMA-BUF 属性。
    /// 返回 true 表示 buffer 支持 DMA-BUF 导出。
    fn wlr_buffer_get_dmabuf(
        buffer: *const std::ffi::c_void,
        attribs: *mut wlr_dmabuf_attributes,
    ) -> bool;
}

/// DMA-BUF buffer 元数据（与 wlr_dmabuf_attributes 同构）。
#[derive(Debug, Clone)]
pub struct BufferMeta {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub stride: u32,
    pub num_planes: u32,
}

/// 从 wlr_buffer 提取 DMA-BUF fd。
///
/// # SAFETY
/// - `buffer` 必须指向有效的 `wlr_buffer`
/// - `wlr_buffer` 必须已锁定且拥有有效的 DMA-BUF
pub unsafe fn extract_dmabuf_fds(
    buffer: *mut std::ffi::c_void,
) -> Result<Vec<RawFd>, Box<dyn std::error::Error>> {
    let mut attribs = wlr_dmabuf_attributes {
        width: 0,
        height: 0,
        format: 0,
        n_planes: 0,
        fd: [-1; WLR_DMABUF_MAX_PLANES],
        offset: [0; WLR_DMABUF_MAX_PLANES],
        stride: [0; WLR_DMABUF_MAX_PLANES],
        modifier: 0,
    };

    if !unsafe { wlr_buffer_get_dmabuf(buffer as *const _, &mut attribs) } {
        return Err("buffer does not support DMA-BUF export".into());
    }

    let count = attribs.n_planes as usize;
    if count == 0 || count > WLR_DMABUF_MAX_PLANES {
        return Err("invalid plane count".into());
    }

    let mut fds = Vec::with_capacity(count);
    for i in 0..count {
        let fd = attribs.fd[i];
        if fd < 0 {
            return Err(format!("invalid fd at plane {}", i).into());
        }
        // dup 以确保所有权转移
        let duped = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if duped < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        fds.push(duped);
    }

    Ok(fds)
}

/// 获取 wlr_buffer 的元数据（不提取 fd）。
///
/// # SAFETY
/// - `buffer` 必须指向有效的 `wlr_buffer`
pub unsafe fn buffer_metadata(
    buffer: *mut std::ffi::c_void,
) -> Result<BufferMeta, Box<dyn std::error::Error>> {
    let mut attribs = wlr_dmabuf_attributes {
        width: 0,
        height: 0,
        format: 0,
        n_planes: 0,
        fd: [-1; WLR_DMABUF_MAX_PLANES],
        offset: [0; WLR_DMABUF_MAX_PLANES],
        stride: [0; WLR_DMABUF_MAX_PLANES],
        modifier: 0,
    };

    if !unsafe { wlr_buffer_get_dmabuf(buffer as *const _, &mut attribs) } {
        return Err("buffer does not support DMA-BUF export".into());
    }

    // 使用第一个 plane 的 stride 作为主 stride
    let stride = if attribs.n_planes > 0 {
        attribs.stride[0]
    } else {
        0
    };

    Ok(BufferMeta {
        width: attribs.width as u32,
        height: attribs.height as u32,
        format: attribs.format,
        stride,
        num_planes: attribs.n_planes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribs_struct_size() {
        assert_eq!(std::mem::size_of::<wlr_dmabuf_attributes>(), 72);
    }

    #[test]
    fn attribs_struct_offsets() {
        let a = wlr_dmabuf_attributes {
            width: 0,
            height: 0,
            format: 0,
            n_planes: 0,
            fd: [-1; 4],
            offset: [0; 4],
            stride: [0; 4],
            modifier: 0,
        };
        let base = &a as *const _ as usize;
        assert_eq!(&a.width as *const _ as usize - base, 0);
        assert_eq!(&a.height as *const _ as usize - base, 4);
        assert_eq!(&a.format as *const _ as usize - base, 8);
        assert_eq!(&a.n_planes as *const _ as usize - base, 12);
        assert_eq!(&a.fd as *const _ as usize - base, 16);
        assert_eq!(&a.offset as *const _ as usize - base, 32);
        assert_eq!(&a.stride as *const _ as usize - base, 48);
        assert_eq!(&a.modifier as *const _ as usize - base, 64);
    }
}

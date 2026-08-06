use std::num::NonZeroUsize;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

// ── Single-buffer SHM frame transport ──
//
// Zero-copy transport is impossible for SHM buffers (smithay does not
// expose the client pool fd), so this module owns ONE resident memfd that
// holds the current frame. KWin's SHM contents are copied in at commit
// time (a single memcpy, no damage tracking — KWin repaints the whole
// buffer per frame); the App consumes the fd and replies with RELEASE
// before this buffer is rewritten (natural back-pressure). dmabuf frames
// (KWin GPU rendering) bypass this entirely — their fd is forwarded
// directly (zero copy).

pub struct FrameMem {
    memfd: OwnedFd,
    /// Resident RW mapping of the memfd.
    map: *mut u8,
    size: usize,
    width: u32,
    height: u32,
}

// SAFETY: `map` points into a private memfd; FrameMem is used only from the
// compositor thread.
unsafe impl Send for FrameMem {}

impl FrameMem {
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        let size = width as usize * height as usize * 4;
        let memfd = nix::sys::memfd::memfd_create(
            "wl-frame",
            nix::sys::memfd::MFdFlags::MFD_CLOEXEC | nix::sys::memfd::MFdFlags::MFD_ALLOW_SEALING,
        )
        .map_err(|e| format!("memfd_create failed: {e}"))?;
        nix::unistd::ftruncate(&memfd, size as _)
            .map_err(|e| format!("ftruncate failed: {e}"))?;
        let map = unsafe {
            nix::sys::mman::mmap(
                None,
                NonZeroUsize::new(size).unwrap(),
                nix::sys::mman::ProtFlags::PROT_READ | nix::sys::mman::ProtFlags::PROT_WRITE,
                nix::sys::mman::MapFlags::MAP_SHARED,
                &memfd,
                0,
            )
        }
        .map_err(|e| format!("mmap failed: {e}"))?
        .as_ptr() as *mut u8;
        Ok(Self { memfd, map, size, width, height })
    }

    /// Ensure the buffer matches the frame dimensions, rebuilding (fresh
    /// memfd, atomic swap) when they change.
    pub fn set_dimensions(&mut self, width: u32, height: u32) -> Result<(), String> {
        if self.width == width && self.height == height {
            return Ok(());
        }
        let replacement = Self::new(width, height)?;
        // SAFETY: self.map is a live mapping of self.size (munmap exactly
        // once, then drop the old memfd).
        unsafe {
            nix::sys::mman::munmap(
                std::ptr::NonNull::new(self.map as *mut std::ffi::c_void).unwrap(),
                self.size,
            )
            .ok();
        }
        *self = replacement;
        Ok(())
    }

    /// Copy the given pixels (full frame, `width*height*4` bytes) into the
    /// resident memfd and return a dup'd fd for SCM_RIGHTS transfer.
    /// Panics are avoided: a size mismatch logs and returns None.
    pub fn push(&mut self, pixels: &[u8], width: u32, height: u32) -> Option<OwnedFd> {
        let needed = width as usize * height as usize * 4;
        if pixels.len() != needed || self.size < needed {
            tracing::warn!(
                data_len = pixels.len(),
                needed,
                buf_size = self.size,
                "frame size mismatch — dropped"
            );
            return None;
        }
        // SAFETY: map is a live mapping of at least `needed` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), self.map, needed);
        }
        let raw = unsafe { libc::dup(self.memfd.as_raw_fd()) };
        if raw >= 0 {
            Some(unsafe { OwnedFd::from_raw_fd(raw) })
        } else {
            None
        }
    }
}

impl Drop for FrameMem {
    fn drop(&mut self) {
        // SAFETY: self.map is a live mapping of self.size.
        unsafe {
            nix::sys::mman::munmap(
                std::ptr::NonNull::new(self.map as *mut std::ffi::c_void).unwrap(),
                self.size,
            )
            .ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn push_writes_full_frame_to_fd() {
        let mut mem = FrameMem::new(4, 4).unwrap();
        let mut pixels = vec![0u8; 4 * 4 * 4];
        for (i, b) in pixels.iter_mut().enumerate() {
            *b = i as u8;
        }
        let fd = mem.push(&pixels, 4, 4).expect("push");
        let st = unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            assert_eq!(libc::fstat(fd.as_raw_fd(), &mut st), 0);
            st
        };
        assert_eq!(st.st_size as usize, 4 * 4 * 4);
        let rp = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                st.st_size as usize,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        assert_ne!(rp, libc::MAP_FAILED);
        let bytes = unsafe { std::slice::from_raw_parts(rp as *const u8, st.st_size as usize) };
        assert_eq!(bytes, pixels.as_slice());
        unsafe { libc::munmap(rp, st.st_size as usize) };
    }

    #[test]
    fn set_dimensions_rebuilds_larger() {
        let mut mem = FrameMem::new(4, 4).unwrap();
        mem.set_dimensions(8, 8).unwrap();
        assert_eq!(mem.size, 8 * 8 * 4);
        let pixels = vec![0xABu8; 8 * 8 * 4];
        let fd = mem.push(&pixels, 8, 8).expect("push after grow");
        let st = unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            assert_eq!(libc::fstat(fd.as_raw_fd(), &mut st), 0);
            st
        };
        assert_eq!(st.st_size as usize, 8 * 8 * 4);
    }

    #[test]
    fn push_size_mismatch_returns_none() {
        let mut mem = FrameMem::new(4, 4).unwrap();
        let pixels = vec![0u8; 4 * 4 * 4 - 1]; // wrong size
        assert!(mem.push(&pixels, 4, 4).is_none());
    }
}

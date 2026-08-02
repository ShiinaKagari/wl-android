use std::num::NonZeroUsize;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub struct FrameCache {
    buffers: Vec<OwnedFd>,
    sizes: Vec<usize>,
    next: usize,
    current: usize,
    seq: u64,
    width: u32,
    height: u32,
}

impl FrameCache {
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        let size = width as usize * height as usize * 4;
        let mut buffers = Vec::with_capacity(3);
        let mut sizes = Vec::with_capacity(3);

        for _ in 0..3 {
            let memfd = nix::sys::memfd::memfd_create(
                "wl-frame-cache",
                nix::sys::memfd::MFdFlags::MFD_CLOEXEC
                    | nix::sys::memfd::MFdFlags::MFD_ALLOW_SEALING,
            )
            .map_err(|e| format!("memfd_create failed: {e}"))?;
            nix::unistd::ftruncate(&memfd, size as _)
                .map_err(|e| format!("ftruncate failed: {e}"))?;
            buffers.push(memfd);
            sizes.push(size);
        }

        Ok(Self {
            buffers,
            sizes,
            next: 0,
            current: 0,
            seq: 0,
            width,
            height,
        })
    }

    pub fn push(&mut self, data: &[u8], width: u32, height: u32) -> Option<OwnedFd> {
        let needed = width as usize * height as usize * 4;
        if data.len() != needed {
            tracing::warn!(data_len = data.len(), needed, "frame data size mismatch");
            return None;
        }
        if needed > self.sizes[self.next] {
            if nix::unistd::ftruncate(&self.buffers[self.next], needed as _).is_err() {
                return None;
            }
            self.sizes[self.next] = needed;
        }

        let ptr = unsafe {
            nix::sys::mman::mmap(
                None,
                NonZeroUsize::new(needed).unwrap(),
                nix::sys::mman::ProtFlags::PROT_READ | nix::sys::mman::ProtFlags::PROT_WRITE,
                nix::sys::mman::MapFlags::MAP_SHARED,
                &self.buffers[self.next],
                0,
            )
        };

        let ptr = match ptr {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(err = %e, "mmap failed in push");
                return None;
            }
        };

        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.as_ptr() as *mut u8, needed);
        }
        unsafe {
            nix::sys::mman::munmap(ptr, needed).ok();
        }

        let prev_next = self.next;
        self.current = prev_next;
        self.next = (self.next + 1) % 3;
        self.seq += 1;

        let raw = unsafe { libc::dup(self.buffers[self.current].as_raw_fd()) };
        if raw >= 0 {
            Some(unsafe { OwnedFd::from_raw_fd(raw) })
        } else {
            None
        }
    }

    pub fn current_frame(&self) -> Option<(OwnedFd, u64)> {
        if self.seq == 0 {
            return None;
        }
        let raw = unsafe { libc::dup(self.buffers[self.current].as_raw_fd()) };
        if raw >= 0 {
            Some((unsafe { OwnedFd::from_raw_fd(raw) }, self.seq))
        } else {
            None
        }
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn set_dimensions(&mut self, width: u32, height: u32) {
        if self.width == width && self.height == height {
            return;
        }
        self.width = width;
        self.height = height;
        let new_size = width as usize * height as usize * 4;
        for (i, buf) in self.buffers.iter().enumerate() {
            if nix::unistd::ftruncate(buf, new_size as _).is_ok() {
                self.sizes[i] = new_size;
            }
        }
    }
}

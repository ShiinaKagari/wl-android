#![allow(unused)]
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

// VERBATIM copy of `safe_mmap_len` from
// /home/kagari/Projects/wl-android/android-app/native/src/session.rs
fn safe_mmap_len(fd: &impl AsRawFd, requested: usize) -> usize {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
    if rc != 0 {
        log::error!(
            "fstat(frame fd) failed: {}; treating frame as empty",
            io::Error::last_os_error()
        );
        return 0;
    }
    (st.st_size as usize).min(requested)
}

// VERBATIM copy of `#[cfg(test)] mod tests` from the same file.
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // /proc/self/fd is process-global, so FdCountGuard is only deterministic
    // while no other fd-touching test runs concurrently. Same convention as the
    // crate (frame_cache.rs FD_GUARD_LOCK): a static mutex serializes the
    // guard-bearing tests so they are mutually exclusive even under parallel
    // `cargo test`. Tests not using the guard stay parallel.
    static FD_GUARD_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

    fn fd_guard_lock() -> &'static std::sync::Mutex<()> {
        FD_GUARD_LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    struct FdCountGuard {
        before: usize,
    }

    impl FdCountGuard {
        fn new() -> Self {
            Self { before: Self::count() }
        }
        fn count() -> usize {
            fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0)
        }
    }

    impl Drop for FdCountGuard {
        fn drop(&mut self) {
            let after = Self::count();
            assert!(after <= self.before, "fd leak: {0} -> {1}", self.before, after);
        }
    }

    fn memfd_of_size(size: usize) -> OwnedFd {
        let name = CString::new("fstat-guard-test").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create failed: {}", io::Error::last_os_error());
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let rc = unsafe { libc::ftruncate(fd, size as libc::off_t) };
        assert_eq!(rc, 0, "ftruncate failed: {}", io::Error::last_os_error());
        owned
    }

    #[test]
    fn fstat_guard_prevents_oversized_mmap() {
        // 64-byte fd backing a frame claiming width*height*4 = 256 bytes.
        // Mapping 256 bytes past EOF would SIGBUS on read; the guard must
        // clamp the mmap length to the fd's real size (64).
        let _serial = fd_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _g = FdCountGuard::new();
        let fd = memfd_of_size(64);
        let safe = safe_mmap_len(&fd, 64 * 4);
        assert_eq!(safe, 64, "must clamp to fstat size, not requested size");
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                safe,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED, "mmap of clamped size failed");
        let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, safe) };
        assert_eq!(slice.to_vec().len(), 64);
        unsafe { libc::munmap(ptr, safe); }
    }

    #[test]
    fn fstat_guard_fd_larger_than_requested() {
        let _serial = fd_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _g = FdCountGuard::new();
        let fd = memfd_of_size(4096);
        assert_eq!(safe_mmap_len(&fd, 256), 256);
    }

    struct InvalidFd;

    impl AsRawFd for InvalidFd {
        fn as_raw_fd(&self) -> std::os::raw::c_int {
            -1
        }
    }

    #[test]
    fn fstat_guard_failure_returns_zero() {
        let _serial = fd_guard_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _g = FdCountGuard::new();
        assert_eq!(safe_mmap_len(&InvalidFd, 100), 0);
    }
}

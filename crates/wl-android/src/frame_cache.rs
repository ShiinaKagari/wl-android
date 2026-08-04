use std::num::NonZeroUsize;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

// ── TODO 31: DEBUG-ONLY (SHM/CPU fallback) ──
//
// This module is the retired SHM/CPU frame path. It is kept ONLY as the
// LAND_MODE=shm debug fallback (state.rs `shm_path_enabled`): with that env
// set, commit pushes pixel frames here and sends memfd-backed pixel-fd frames
// to the App. In the default (blit) mode the SHM branch is gated off and this
// cache stays None — KWin must produce dmabufs (the doctor/deploy scripts set
// the env). Do not extend; the production path is blit.rs.
//
// PERF: the three memfds are mapped ONCE at pool build time and kept mapped
// for the pool's lifetime (PERF-12). The previous implementation mmap'd +
// munmap'd 32MB per push, which cost ~20ms/frame on top of the memcpy itself
// (page-table churn, TLB shootdowns). Writing straight into the resident
// mapping cuts the push cost to the raw memcpy; `push_from` additionally lets
// the caller copy directly from the SHM pool (single copy, no intermediate
// Vec).

pub struct FrameCache {
    buffers: Vec<OwnedFd>,
    /// Resident RW mappings of each memfd (same order as `buffers`).
    maps: Vec<*mut u8>,
    sizes: Vec<usize>,
    next: usize,
    current: usize,
    seq: u64,
    width: u32,
    height: u32,
}

// SAFETY: `maps` are pointers into private memfds; FrameCache is not Send/Sync
// by default because of them, and it is only used from the compositor thread.
unsafe impl Send for FrameCache {}

impl FrameCache {
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        let size = width as usize * height as usize * 4;
        let mut buffers = Vec::with_capacity(3);
        let mut maps = Vec::with_capacity(3);
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
            let ptr = Self::map_fd(&memfd, size)?;
            buffers.push(memfd);
            maps.push(ptr);
            sizes.push(size);
        }

        Ok(Self {
            buffers,
            maps,
            sizes,
            next: 0,
            current: 0,
            seq: 0,
            width,
            height,
        })
    }

    fn map_fd(fd: &OwnedFd, size: usize) -> Result<*mut u8, String> {
        // SAFETY: nix wraps mmap(2); the returned pointer is checked against
        // MAP_FAILED by nix itself and handed to the caller for ownership.
        let ptr = unsafe {
            nix::sys::mman::mmap(
                None,
                NonZeroUsize::new(size).unwrap(),
                nix::sys::mman::ProtFlags::PROT_READ | nix::sys::mman::ProtFlags::PROT_WRITE,
                nix::sys::mman::MapFlags::MAP_SHARED,
                fd,
                0,
            )
        }
        .map_err(|e| format!("mmap failed: {e}"))?;
        Ok(ptr.as_ptr() as *mut u8)
    }

    fn unmap(idx: usize, ptr: *mut u8, size: usize) {
        // SAFETY: `ptr` was produced by map_fd for this same size and is still
        // owned by the pool (callers unmap exactly once per live mapping).
        unsafe {
            nix::sys::mman::munmap(
                std::ptr::NonNull::new(ptr as *mut std::ffi::c_void).unwrap(),
                size,
            )
            .ok();
        }
        let _ = idx;
    }

    /// Grow buffer `idx` in place: unmap, ftruncate larger, remap. Shrinking
    /// is never done here (F-15 forbids in-place shrink — outstanding App
    /// mmaps would extend past EOF and SIGBUS).
    fn ensure_size(&mut self, idx: usize, needed: usize) -> bool {
        if needed <= self.sizes[idx] {
            return true;
        }
        if nix::unistd::ftruncate(&self.buffers[idx], needed as _).is_err() {
            return false;
        }
        match Self::map_fd(&self.buffers[idx], needed) {
            Ok(ptr) => {
                Self::unmap(idx, self.maps[idx], self.sizes[idx]);
                self.maps[idx] = ptr;
                self.sizes[idx] = needed;
                true
            }
            Err(e) => {
                tracing::warn!(err = %e, "remap failed during grow");
                false
            }
        }
    }

    /// Push pixel data into the next buffer of the triple-buffer rotation.
    /// Returns a dup'd fd for SCM_RIGHTS transfer (the caller owns it).
    pub fn push(&mut self, data: &[u8], width: u32, height: u32) -> Option<OwnedFd> {
        let needed = width as usize * height as usize * 4;
        if data.len() != needed {
            tracing::warn!(data_len = data.len(), needed, "frame data size mismatch");
            return None;
        }
        if !self.ensure_size(self.next, needed) {
            return None;
        }
        // SAFETY: maps[next] is a valid mapping of at least `needed` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), self.maps[self.next], needed);
        }
        self.rotate(needed)
    }

    /// Push with a writer closure: the caller copies the frame into the
    /// resident mapping itself (e.g. directly out of the SHM pool), so the
    /// intermediate Vec allocation + second memcpy are eliminated (PERF-12).
    ///
    /// The closure receives the full `width * height * 4` writable slice.
    /// On size mismatch the frame is dropped (None) like `push`.
    pub fn push_from<F: FnOnce(&mut [u8])>(
        &mut self,
        width: u32,
        height: u32,
        write: F,
    ) -> Option<OwnedFd> {
        let needed = width as usize * height as usize * 4;
        if !self.ensure_size(self.next, needed) {
            return None;
        }
        // SAFETY: maps[next] is a valid mapping of at least `needed` bytes.
        let dst = unsafe { std::slice::from_raw_parts_mut(self.maps[self.next], needed) };
        write(dst);
        self.rotate(needed)
    }

    /// Advance the rotation after a successful write and hand out a dup'd fd.
    fn rotate(&mut self, _needed: usize) -> Option<OwnedFd> {
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

    pub fn current_frame(&self) -> Option<(OwnedFd, u64, u32, u32)> {
        if self.seq == 0 {
            return None;
        }
        let raw = unsafe { libc::dup(self.buffers[self.current].as_raw_fd()) };
        if raw >= 0 {
            Some((unsafe { OwnedFd::from_raw_fd(raw) }, self.seq, self.width, self.height))
        } else {
            None
        }
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// F-15: 尺寸变更原子重建 (atomic rebuild on dimension change).
    ///
    /// Any dimension change allocates a FRESH 3-memfd pool at the new size and
    /// atomically swaps it in. The previous code ftruncate'd the memfds in place;
    /// shrinking while the App still holds an mmap of the old size leaves that
    /// mapping extending past EOF, and a read SIGBUSes (the root cause this fixes).
    ///
    /// The rebuild is all-or-nothing: the new pool is built fully on the side, and
    /// only committed to `self` once every memfd is created, truncated AND mapped.
    /// On any allocation failure the old pool, sizes and dimensions are left
    /// untouched, so outstanding App mmaps stay valid. `seq` is intentionally NOT
    /// reset — it stays monotonic across resizes. `next`/`current` reset to 0 so
    /// the new pool is used from buffer 0. Old pool fds drop naturally once
    /// replaced (their mappings are unmapped here; App-held dup'd fds remain
    /// valid — munmap of our mapping does not affect theirs).
    pub fn set_dimensions(&mut self, width: u32, height: u32) {
        if self.width == width && self.height == height {
            return;
        }

        let size = width as usize * height as usize * 4;
        let mut buffers = Vec::with_capacity(3);
        let mut maps = Vec::with_capacity(3);
        let mut sizes = Vec::with_capacity(3);

        for _ in 0..3 {
            let memfd = match nix::sys::memfd::memfd_create(
                "wl-frame-cache",
                nix::sys::memfd::MFdFlags::MFD_CLOEXEC
                    | nix::sys::memfd::MFdFlags::MFD_ALLOW_SEALING,
            ) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(err = %e, "memfd_create failed during resize; keeping old pool");
                    return;
                }
            };
            if nix::unistd::ftruncate(&memfd, size as _).is_err() {
                tracing::warn!("ftruncate failed during resize; keeping old pool");
                return;
            }
            match Self::map_fd(&memfd, size) {
                Ok(ptr) => {
                    buffers.push(memfd);
                    maps.push(ptr);
                    sizes.push(size);
                }
                Err(e) => {
                    tracing::warn!(err = %e, "mmap failed during resize; keeping old pool");
                    return;
                }
            }
        }

        // Atomic commit: only now that the whole pool is ready do we touch self.
        for i in 0..self.maps.len() {
            Self::unmap(i, self.maps[i], self.sizes[i]);
        }
        self.buffers = buffers;
        self.maps = maps;
        self.sizes = sizes;
        self.next = 0;
        self.current = 0;
        self.width = width;
        self.height = height;
        // seq deliberately not reset: monotonic across resizes (F-15).
    }
}

impl Drop for FrameCache {
    fn drop(&mut self) {
        for i in 0..self.maps.len() {
            Self::unmap(i, self.maps[i], self.sizes[i]);
        }
    }
}

// ── F-15: 尺寸变更原子重建 (size changes allocate a fresh memfd triple-buffer) ──

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    // X-04: fd leak guard. Mirrors wl_android_common::testutil::fd_util::FdCountGuard,
    // which is not reachable from this crate (testutil is cfg(test)-only in
    // wl-android-common and that flag is not active for dependency crates).
    //
    // /proc/self/fd is process-global, so this guard is only deterministic while no
    // other fd-touching test runs concurrently. That is the crate's established
    // convention too: wl-android-common F-03 ignores its guard test with
    // "requires --test-threads=1 due to global fd counting via /proc/self/fd".
    // The static mutex serializes this module's own tests; combine with
    // `--test-threads=1` for a fully deterministic run.
    static FD_GUARD_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

    fn fd_guard_lock() -> &'static std::sync::Mutex<()> {
        FD_GUARD_LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    struct FdCountGuard {
        initial: usize,
        label: &'static str,
    }

    impl FdCountGuard {
        fn new(label: &'static str) -> Self {
            let initial = std::fs::read_dir("/proc/self/fd")
                .map(|entries| entries.count())
                .unwrap_or(0);
            Self { initial, label }
        }
    }

    impl Drop for FdCountGuard {
        fn drop(&mut self) {
            let current = std::fs::read_dir("/proc/self/fd")
                .map(|entries| entries.count())
                .unwrap_or(0);
            assert_eq!(
                current, self.initial,
                "FdCountGuard [{}]: fd count changed: {} != {} (leak detected)",
                self.label, current, self.initial
            );
        }
    }

    fn fstat_size(fd: &OwnedFd) -> i64 {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
        assert_eq!(rc, 0, "fstat failed");
        st.st_size
    }

    fn fstat_ino(fd: &OwnedFd) -> libc::ino_t {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
        assert_eq!(rc, 0, "fstat failed");
        st.st_ino
    }

    fn frame_data(w: u32, h: u32, byte: u8) -> Vec<u8> {
        vec![byte; w as usize * h as usize * 4]
    }

    // F-15: shrink must rebuild the pool, never ftruncate in place. The App may still
    // hold an mmap of the OLD size; an in-place shrink makes that mapping extend past
    // EOF, and reading it SIGBUSes.
    #[test]
    fn dimension_change_reallocates_not_shrinks() {
        let _serial = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("dimension_change_reallocates_not_shrinks");
        let mut cache = FrameCache::new(100, 100).unwrap();

        let old_fd = cache.push(&frame_data(100, 100, 1), 100, 100).expect("push 100x100");
        let old_ino = fstat_ino(&old_fd);
        let prev_seq = cache.seq();
        assert!(prev_seq >= 1);

        // Simulate the App holding an mmap of the OLD size across the resize.
        let map_len = 100usize * 100 * 4;
        let map_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                old_fd.as_raw_fd(),
                0,
            )
        };
        assert_ne!(map_ptr, libc::MAP_FAILED, "mmap of old-size buffer failed");

        cache.set_dimensions(50, 50);

        // Last byte of the old mapping lies beyond a 50x50 EOF (10000). If the memfd had
        // been ftruncate-shrunk in place, this read SIGBUSes — the F-15 root cause.
        let byte = unsafe { *(map_ptr as *const u8).add(map_len - 1) };
        assert_eq!(byte, 1, "stale mmap must stay readable after resize (no in-place shrink)");
        unsafe { libc::munmap(map_ptr, map_len) };

        // (a) push at the new size succeeds
        let new_fd = cache.push(&frame_data(50, 50, 7), 50, 50).expect("push 50x50 after resize");
        // (b) the pushed buffer is exactly the new size (fresh memfd, not shrunk in place)
        assert_eq!(fstat_size(&new_fd), 50 * 50 * 4, "new buffer must be 50x50x4 bytes");
        // rebuilt pool → the returned fd is a NEW memfd, not the old one resized
        assert_ne!(fstat_ino(&new_fd), old_ino, "resize must allocate fresh memfds");
        // (c) seq is monotonic, never reset by the resize
        assert!(cache.seq() >= prev_seq, "seq must stay monotonic across resize");

        // current_frame reports the NEW dimensions after the size change
        let (cur, _, w, h) = cache.current_frame().expect("current frame after resize");
        assert_eq!((w, h), (50, 50));
        assert_eq!(fstat_size(&cur), 50 * 50 * 4);
    }

    #[test]
    fn set_dimensions_noop_same_size() {
        let _serial = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("set_dimensions_noop_same_size");
        let mut cache = FrameCache::new(100, 100).unwrap();

        // Fill the whole pool so `next` wraps back to buffer 0.
        let mut inos = Vec::new();
        for _ in 0..3 {
            inos.push(fstat_ino(&cache.push(&frame_data(100, 100, 1), 100, 100).unwrap()));
        }
        let seq_before = cache.seq();
        assert_eq!(seq_before, 3);

        cache.set_dimensions(100, 100);

        assert_eq!(cache.seq(), seq_before, "no-op resize must not touch seq");
        let fd = cache.push(&frame_data(100, 100, 2), 100, 100).unwrap();
        assert_eq!(fstat_size(&fd), 100 * 100 * 4, "buffer size unchanged");
        // Same pool, index rotation untouched: the 4th push lands on buffer 0 again.
        assert_eq!(fstat_ino(&fd), inos[0], "no-op resize must not rebuild the pool");
        let (_, _, w, h) = cache.current_frame().expect("current frame");
        assert_eq!((w, h), (100, 100));
    }

    #[test]
    fn set_dimensions_grow_creates_larger_buffers() {
        let _serial = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("set_dimensions_grow_creates_larger_buffers");
        let mut cache = FrameCache::new(100, 100).unwrap();
        let old_fd = cache.push(&frame_data(100, 100, 1), 100, 100).unwrap();
        let old_ino = fstat_ino(&old_fd);

        cache.set_dimensions(200, 150);

        let fd = cache.push(&frame_data(200, 150, 2), 200, 150).unwrap();
        assert_eq!(fstat_size(&fd), 200 * 150 * 4, "grow must yield 200x150x4 buffers");
        assert_ne!(fstat_ino(&fd), old_ino, "grow must allocate a fresh pool");
        let (cur, _, w, h) = cache.current_frame().expect("current frame");
        assert_eq!((w, h), (200, 150));
        assert_eq!(fstat_size(&cur), 200 * 150 * 4);
    }

    #[test]
    fn set_dimensions_keeps_seq_monotonic() {
        let _serial = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("set_dimensions_keeps_seq_monotonic");
        let mut cache = FrameCache::new(100, 100).unwrap();
        assert_eq!(cache.seq(), 0);

        cache.push(&frame_data(100, 100, 1), 100, 100).unwrap();
        assert_eq!(cache.seq(), 1);

        cache.set_dimensions(50, 50);
        assert_eq!(cache.seq(), 1, "resize must not reset seq");
        cache.push(&frame_data(50, 50, 2), 50, 50).unwrap();
        assert_eq!(cache.seq(), 2);

        cache.set_dimensions(200, 200);
        assert_eq!(cache.seq(), 2, "resize must not reset seq");
        cache.push(&frame_data(200, 200, 3), 200, 200).unwrap();
        assert_eq!(cache.seq(), 3, "seq strictly monotonic across resizes");
    }

    // PERF-12: push_from writes through the resident mapping — the returned
    // fd must expose exactly the bytes the closure wrote (no intermediate Vec).
    #[test]
    fn push_from_writes_through_resident_mapping() {
        let _serial = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("push_from_writes_through_resident_mapping");
        let mut cache = FrameCache::new(4, 4).unwrap();

        let fd = cache
            .push_from(4, 4, |dst| {
                assert_eq!(dst.len(), 4 * 4 * 4);
                // Fill with a recognizable pattern: each row = row index.
                for y in 0..4u8 {
                    for x in 0..16usize {
                        dst[y as usize * 16 + x] = y;
                    }
                }
            })
            .expect("push_from");

        // Read back through the dup'd fd and verify the pattern.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                64,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, 64) };
        for y in 0..4u8 {
            for x in 0..16usize {
                assert_eq!(bytes[y as usize * 16 + x], y, "row {} col {}", y, x);
            }
        }
        unsafe { libc::munmap(ptr, 64) };
    }

    // PERF-12: the resident mapping must stay writable across rotations
    // (buffer 0 gets reused on the 4th push and must not be stale-locked).
    #[test]
    fn push_from_rotation_wraps_cleanly() {
        let _serial = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("push_from_rotation_wraps_cleanly");
        let mut cache = FrameCache::new(2, 2).unwrap();

        let mut inos = Vec::new();
        for i in 0..5u8 {
            let fd = cache
                .push_from(2, 2, |dst| {
                    for b in dst.iter_mut() {
                        *b = i;
                    }
                })
                .expect("push_from");
            inos.push(fstat_ino(&fd));
            assert_eq!(cache.seq(), (i + 1) as u64);
        }

        // 4th push wraps to buffer 0; 5th to buffer 1 — inos repeat in rotation.
        assert_eq!(inos[3], inos[0], "4th push must reuse buffer 0");
        assert_eq!(inos[4], inos[1], "5th push must reuse buffer 1");
        assert_eq!(cache.seq(), 5, "seq counts every push");
    }
}

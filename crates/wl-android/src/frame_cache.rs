use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

// ── SHM frame cache (the only frame path) ──
//
// This module is the SHM frame path. KWin (nested compositor) commits SHM
// buffers; state.rs extracts each frame into a resident memfd pool here and
// ships memfd-backed pixel-fd frames to the App, which presents via
// ANativeWindow_lock. There is no dmabuf/blit path (turnip import of App AHB
// SIGSEGVs on this device — device-verified).
//
// PERF: the memfds are mapped ONCE at pool build time and kept mapped for the
// pool's lifetime (PERF-12). The previous implementation mmap'd + munmap'd
// 32MB per push, which cost ~20ms/frame on top of the memcpy itself
// (page-table churn, TLB shootdowns). Writing straight into the resident
// mapping cuts the push cost to the raw memcpy; `push_from` additionally lets
// the caller copy directly from the SHM pool (single copy, no intermediate
// Vec).
//
// PERF-DAMAGE: two buffers alternate strictly (frame N → buffer 0, N+1 →
// buffer 1, N+2 → buffer 0, ...). Each buffer keeps a `pending` damage list:
// every commit unions its damage rects into BOTH buffers' pending lists, and
// the target buffer copies exactly its accumulated rects out of the fresh
// KWin frame. The target's untouched regions keep their previous content —
// which, because every intermediate frame's damage was accumulated, is
// exactly the current frame there. First write after (re)build is a full
// frame (fresh memfd content is garbage).

/// A rectangle in pixel coordinates (clamped to the frame bounds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    fn full(w: u32, h: u32) -> Self {
        Self { x: 0, y: 0, w, h }
    }

    fn contains(&self, other: &Rect) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.x + other.w <= self.x + self.w
            && other.y + other.h <= self.y + self.h
    }

    fn mergeable(&self, other: &Rect) -> bool {
        // Overlapping or adjacent (touching) rects merge into one.
        let overlap_x = self.x < other.x + other.w && other.x < self.x + self.w;
        let overlap_y = self.y < other.y + other.h && other.y < self.y + self.h;
        overlap_x && overlap_y
    }

    fn merged(&self, other: &Rect) -> Rect {
        let x1 = self.x.min(other.x);
        let y1 = self.y.min(other.y);
        let x2 = (self.x + self.w).max(other.x + other.w);
        let y2 = (self.y + self.h).max(other.y + other.h);
        Rect { x: x1, y: y1, w: x2 - x1, h: y2 - y1 }
    }
}

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
    /// Per-buffer accumulated damage since its last full write. Every commit
    /// unions its rects into all buffers; the target copies its own list.
    pending: Vec<Vec<Rect>>,
    /// A buffer whose content has never been written (fresh after build or
    /// resize) must be filled entirely on first use.
    valid: Vec<bool>,
    /// FIFO of buffers whose pixel fd is currently held by the App (sent but
    /// not yet released). A release pops the OLDEST entry — the App consumes
    /// frames in order, so FIFO ownership tracking is exact. A buffer still
    /// in this queue must not be written (the App may be reading it).
    in_flight: VecDeque<usize>,
}

// SAFETY: `maps` are pointers into private memfds; FrameCache is not Send/Sync
// by default because of them, and it is only used from the compositor thread.
unsafe impl Send for FrameCache {}

impl FrameCache {
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        let size = width as usize * height as usize * 4;
        let mut buffers = Vec::with_capacity(2);
        let mut maps = Vec::with_capacity(2);
        let mut sizes = Vec::with_capacity(2);

        for _ in 0..2 {
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
            pending: vec![Vec::new(), Vec::new()],
            valid: vec![false, false],
            in_flight: VecDeque::new(),
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

    /// Push pixel data into the next buffer of the double-buffer rotation.
    /// Returns a dup'd fd for SCM_RIGHTS transfer (the caller owns it).
    /// Production uses `push_from`/`push_damaged`; `push` is exercised by the
    /// F-15 tests.
    #[allow(dead_code)]
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
        self.after_full_write();
        self.rotate(needed)
    }

    /// Push with a writer closure: the caller copies the frame into the
    /// resident mapping itself (e.g. directly out of the SHM pool), so the
    /// intermediate Vec allocation + second memcpy are eliminated (PERF-12).
    ///
    /// The closure receives the full `width * height * 4` writable slice.
    /// On size mismatch the frame is dropped (None) like `push`.
    /// Production uses `push_damaged`; kept for the PERF-12 tests.
    #[allow(dead_code)]
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
        self.after_full_write();
        self.rotate(needed)
    }

    /// PERF-DAMAGE: write only the damaged rects into the next buffer. The
    /// caller's closure receives the target slice and the EFFECTIVE rect
    /// list (clamped to the frame; a full frame on the first write after a
    /// build/resize) and copies each rect out of the fresh KWin frame. The
    /// target's untouched regions keep their previous content, which — with
    /// the per-buffer pending accumulation — equals the current frame there.
    ///
    /// Every commit must pass its damage rects here (possibly empty: a
    /// zero-damage commit still advances the rotation so the App sees a
    /// fresh fd for the unchanged frame).
    ///
    /// Returns None when the target buffer is still held by the App (in
    /// flight): the frame is DROPPED (latest-wins) — the damage is still
    /// accumulated so the next writable buffer catches up, but no fd is sent.
    /// On success the written buffer is automatically marked in flight (the
    /// App must release it before it can be written again).
    pub fn push_damaged<F: FnOnce(&mut [u8], &[Rect])>(
        &mut self,
        width: u32,
        height: u32,
        rects: &[Rect],
        write: F,
    ) -> Option<OwnedFd> {
        let needed = width as usize * height as usize * 4;
        if !self.ensure_size(self.next, needed) {
            return None;
        }

        // Union the new damage into EVERY buffer's pending list FIRST — this
        // must happen even when the frame is dropped below, so the next
        // writable buffer repaints this frame's changes too (latest-wins
        // must not lose pixels, only frame pacing).
        for pending in &mut self.pending {
            for r in rects {
                Self::union_rect(pending, *r);
            }
        }

        // The App still holds the target buffer's fd — writing now would
        // race its read. Drop the frame (latest-wins); the damage above is
        // already recorded for the next writable turn.
        if self.in_flight.contains(&self.next) {
            return None;
        }

        // First write after build/resize: the memfd content is garbage, so
        // the effective region is the whole frame regardless of `rects`.
        let effective: Vec<Rect> = if !self.valid[self.next] {
            vec![Rect::full(self.width, self.height)]
        } else {
            std::mem::take(&mut self.pending[self.next])
        };

        // SAFETY: maps[next] is a valid mapping of at least `needed` bytes.
        let dst = unsafe { std::slice::from_raw_parts_mut(self.maps[self.next], needed) };
        write(dst, &effective);
        self.pending[self.next].clear();
        self.valid[self.next] = true;
        let fd = self.rotate(needed);
        if fd.is_some() {
            self.in_flight.push_back(self.current);
        }
        fd
    }

    /// The App finished consuming the most recently received frame: the
    /// OLDEST in-flight buffer is freed (FIFO — the App consumes in order).
    /// Extra releases (when no buffer is in flight) are ignored.
    pub fn on_release(&mut self) {
        self.in_flight.pop_front();
    }

    /// Forget every in-flight buffer (session teardown / App reconnect): the
    /// old App's outstanding fds are gone, so their ownership marks must not
    /// survive into the new session's FIFO accounting.
    pub fn reset_in_flight(&mut self) {
        self.in_flight.clear();
    }

    /// Insert `r` into `list`, merging with any overlapping/adjacent rect so
    /// the list stays small (damage from consecutive frames coalesces).
    fn union_rect(list: &mut Vec<Rect>, r: Rect) {
        for existing in list.iter_mut() {
            if existing.contains(&r) {
                return;
            }
            if existing.mergeable(&r) {
                *existing = existing.merged(&r);
                return;
            }
        }
        list.push(r);
    }

    /// Bookkeeping after a FULL frame write into the target buffer: only the
    /// target's pending list is satisfied (its content now IS the current
    /// frame). Every OTHER buffer must learn about the whole frame — its
    /// content is stale everywhere, so its pending list gets the full-frame
    /// rect to force a complete repaint on its next turn. (Without this, a
    /// later partial write into that buffer would leave regions from before
    /// the full frame untouched.)
    fn after_full_write(&mut self) {
        let full = Rect::full(self.width, self.height);
        for (i, pending) in self.pending.iter_mut().enumerate() {
            if i == self.next {
                pending.clear();
            } else {
                Self::union_rect(pending, full);
            }
        }
        self.valid[self.next] = true;
    }

    /// Advance the rotation after a successful write and hand out a dup'd fd.
    fn rotate(&mut self, _needed: usize) -> Option<OwnedFd> {
        let prev_next = self.next;
        self.current = prev_next;
        self.next = (self.next + 1) % 2;
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

    /// Mark the current buffer as handed to the App (used by the reconnect
    /// replay path, which hands out `current_frame`'s dup'd fd without going
    /// through `push_damaged`). No-op when the buffer is already in flight.
    pub fn mark_current_in_flight(&mut self) {
        if !self.in_flight.contains(&self.current) {
            self.in_flight.push_back(self.current);
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
        let mut buffers = Vec::with_capacity(2);
        let mut maps = Vec::with_capacity(2);
        let mut sizes = Vec::with_capacity(2);

        for _ in 0..2 {
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
        // Fresh pool: content is garbage, so every buffer needs a full frame
        // on first use; the old pending lists are meaningless at the new size.
        self.pending = vec![Vec::new(), Vec::new()];
        self.valid = vec![false, false];
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
        for _ in 0..2 {
            inos.push(fstat_ino(&cache.push(&frame_data(100, 100, 1), 100, 100).unwrap()));
        }
        let seq_before = cache.seq();
        assert_eq!(seq_before, 2);

        cache.set_dimensions(100, 100);

        assert_eq!(cache.seq(), seq_before, "no-op resize must not touch seq");
        let fd = cache.push(&frame_data(100, 100, 2), 100, 100).unwrap();
        assert_eq!(fstat_size(&fd), 100 * 100 * 4, "buffer size unchanged");
        // Same pool, index rotation untouched: the 3rd push lands on buffer 0 again.
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
    // (buffer 0 gets reused on the 3rd push and must not be stale-locked).
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

        // 3rd push wraps to buffer 0; 4th to buffer 1 — inos repeat in rotation.
        assert_eq!(inos[2], inos[0], "3rd push must reuse buffer 0");
        assert_eq!(inos[3], inos[1], "4th push must reuse buffer 1");
        assert_eq!(inos[4], inos[0], "5th push must reuse buffer 0");
        assert_eq!(cache.seq(), 5, "seq counts every push");
    }

    fn read_fd(fd: &OwnedFd) -> Vec<u8> {
        let size = fstat_size(fd);
        assert!(size > 0, "fd must be a sized memfd");
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED, "mmap for read-back failed");
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, size as usize) }.to_vec();
        unsafe { libc::munmap(ptr, size as usize) };
        bytes
    }

    fn fill_pattern(w: u32, h: u32, seed: u8) -> Vec<u8> {
        let mut v = vec![0u8; w as usize * h as usize * 4];
        for (i, b) in v.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        v
    }

    fn write_rect(dst: &mut [u8], rect: &Rect, w: u32, src: &[u8]) {
        let stride = w as usize * 4;
        for y in rect.y..rect.y + rect.h {
            let row = y as usize * stride + rect.x as usize * 4;
            let bytes = rect.w as usize * 4;
            dst[row..row + bytes].copy_from_slice(&src[row..row + bytes]);
        }
    }

    // PERF-DAMAGE: the first write after a fresh pool is a FULL frame even
    // when the caller reports a small damage rect (memfd content is garbage).
    #[test]
    fn push_damaged_first_write_is_full_frame() {
        let _serial = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("push_damaged_first_write_is_full_frame");
        let mut cache = FrameCache::new(4, 4).unwrap();
        let full = fill_pattern(4, 4, 1);

        let fd = cache
            .push_damaged(4, 4, &[Rect { x: 0, y: 0, w: 1, h: 1 }], |dst, effective| {
                assert_eq!(effective, &[Rect { x: 0, y: 0, w: 4, h: 4 }], "first write must be full-frame");
                for r in effective {
                    write_rect(dst, r, 4, &full);
                }
            })
            .expect("push_damaged");
        assert_eq!(read_fd(&fd), full, "first frame must contain the full pattern");
    }

    // PERF-DAMAGE: after both buffers hold a full frame, a partial write
    // updates only the damaged rect; untouched regions keep the previous
    // content written into that buffer.
    #[test]
    fn push_damaged_partial_write_preserves_untouched() {
        let _serial = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("push_damaged_partial_write_preserves_untouched");
        let mut cache = FrameCache::new(4, 4).unwrap();
        let frame1 = fill_pattern(4, 4, 1);
        let frame2 = fill_pattern(4, 4, 2);
        let frame3 = fill_pattern(4, 4, 3);

        // Frame 1: full write of pattern 1 into buffer A.
        cache
            .push_damaged(4, 4, &[Rect { x: 0, y: 0, w: 4, h: 4 }], |dst, effective| {
                for r in effective {
                    write_rect(dst, r, 4, &frame1);
                }
            })
            .expect("frame 1");
        // App consumed frame 1's fd — the buffer returns to the writable pool.
        cache.on_release();
        // Frame 2: full write of pattern 2 into buffer B (first write after a
        // fresh pool is always a full frame, even for a small reported rect).
        cache
            .push_damaged(4, 4, &[Rect { x: 0, y: 0, w: 1, h: 1 }], |dst, effective| {
                assert_eq!(effective, &[Rect { x: 0, y: 0, w: 4, h: 4 }], "B's first write must be full-frame");
                for r in effective {
                    write_rect(dst, r, 4, &frame2);
                }
            })
            .expect("frame 2");
        cache.on_release();

        // Frame 3: only the top-left 2x2 rect changed (pattern 3) in buffer A,
        // which already holds frame 1's pattern.
        let rect = Rect { x: 0, y: 0, w: 2, h: 2 };
        let fd = cache
            .push_damaged(4, 4, &[rect], |dst, effective| {
                assert_eq!(effective, &[rect], "partial write must only repaint the damaged rect");
                for r in effective {
                    write_rect(dst, r, 4, &frame3);
                }
            })
            .expect("frame 3");

        let got = read_fd(&fd);
        let mut expected = frame1.clone();
        write_rect(&mut expected, &rect, 4, &frame3);
        assert_eq!(got, expected, "damaged rect updated, untouched regions preserved");
    }

    // PERF-DAMAGE: the alternating double buffer accumulates damage across
    // frames. Buffer A holds frame N; frame N+1 writes B (full, first write);
    // frame N+2 writes A again and must carry BOTH N+1's and N+2's changes
    // (A's untouched regions still show frame N).
    #[test]
    fn push_damaged_alternation_accumulates_both_frames() {
        let _serial = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("push_damaged_alternation_accumulates_both_frames");
        let mut cache = FrameCache::new(4, 4).unwrap();
        let full = fill_pattern(4, 4, 1);

        // Frame N (buffer A): full frame.
        cache
            .push_damaged(4, 4, &[Rect { x: 0, y: 0, w: 4, h: 4 }], |dst, effective| {
                for r in effective {
                    write_rect(dst, r, 4, &full);
                }
            })
            .expect("frame N");

        // Frame N+1 (buffer B): B is a fresh buffer, so this write is a FULL
        // frame of pattern 2 — B does not inherit A's content.
        let f2 = fill_pattern(4, 4, 2);
        cache
            .push_damaged(4, 4, &[Rect { x: 0, y: 0, w: 1, h: 1 }], |dst, effective| {
                assert_eq!(effective, &[Rect { x: 0, y: 0, w: 4, h: 4 }], "B's first write must be full-frame");
                for r in effective {
                    write_rect(dst, r, 4, &f2);
                }
            })
            .expect("frame N+1");
        assert_eq!(read_fd(&cache.current_frame().unwrap().0), f2, "B = full pattern 2");

        // Frame N+2 (buffer A again): A still holds frame N. Every commit
        // unions its damage into BOTH buffers' pending lists, so A's list
        // covers N+1's rect (0,0,1,1) AND this frame's rect — A must repaint
        // everything it missed since its last write (frame N).
        let r1 = Rect { x: 0, y: 0, w: 1, h: 1 };
        let r2 = Rect { x: 2, y: 2, w: 1, h: 1 };
        let f3 = fill_pattern(4, 4, 3);
        // Frame N's buffer is still in flight; N+1's is not. To write A again
        // the App must have released frame N's fd (FIFO: oldest first).
        cache.on_release();
        let fd = cache
            .push_damaged(4, 4, &[r2], |dst, effective| {
                let mut expected = vec![r1, r2];
                expected.sort_by_key(|r| (r.x, r.y));
                let mut eff = effective.to_vec();
                eff.sort_by_key(|r| (r.x, r.y));
                assert_eq!(eff, expected, "A must repaint every rect since its last write (N+1's and this frame's)");
                for r in effective {
                    write_rect(dst, r, 4, &f3);
                }
            })
            .expect("frame N+2");
        let a = read_fd(&fd);
        let mut expected_a = full.clone();
        write_rect(&mut expected_a, &r1, 4, &f3);
        write_rect(&mut expected_a, &r2, 4, &f3);
        assert_eq!(a, expected_a, "buffer A = frame N + rects from N+1 and N+2");
    }

    // PERF-DAMAGE regression: after a FULL frame write into one buffer, the
    // OTHER buffer must repaint its whole area on its next (partial) write —
    // its content is stale everywhere, not just in the damaged rects.
    #[test]
    fn full_write_then_partial_on_other_buffer_repaints_all() {
        let _serial = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("full_write_then_partial_on_other_buffer_repaints_all");
        let mut cache = FrameCache::new(4, 4).unwrap();
        let frame1 = fill_pattern(4, 4, 1);

        // Frame 1: FULL write into buffer A (the production `push_from` path).
        cache
            .push_from(4, 4, |dst| dst.copy_from_slice(&frame1))
            .expect("frame 1 (full)");

        // Frame 2: partial write into buffer B. B was never written, so this
        // is a full frame of pattern 2 — and A's pending list must now carry
        // the full-frame rect (A missed frame 2 entirely).
        let frame2 = fill_pattern(4, 4, 2);
        cache
            .push_damaged(4, 4, &[Rect { x: 1, y: 1, w: 1, h: 1 }], |dst, effective| {
                assert_eq!(effective, &[Rect { x: 0, y: 0, w: 4, h: 4 }], "B's first write must be full-frame");
                for r in effective {
                    write_rect(dst, r, 4, &frame2);
                }
            })
            .expect("frame 2");

        // Frame 3: partial write back into A with a small rect. A's pending
        // accumulated frame 2's rect (1,1) plus this frame's rect (2,2) — A
        // must repaint exactly those (its content = frame 1 everywhere else,
        // and frame 2 only changed (1,1) relative to frame 1).
        let frame3 = fill_pattern(4, 4, 3);
        let fd = cache
            .push_damaged(4, 4, &[Rect { x: 2, y: 2, w: 1, h: 1 }], |dst, effective| {
                let mut expected = vec![Rect { x: 1, y: 1, w: 1, h: 1 }, Rect { x: 2, y: 2, w: 1, h: 1 }];
                expected.sort_by_key(|r| (r.x, r.y));
                let mut eff = effective.to_vec();
                eff.sort_by_key(|r| (r.x, r.y));
                assert_eq!(eff, expected, "A repaints every rect it missed since its last write");
                for r in effective {
                    write_rect(dst, r, 4, &frame3);
                }
            })
            .expect("frame 3");
        let mut expected_a = frame1.clone();
        write_rect(&mut expected_a, &Rect { x: 1, y: 1, w: 1, h: 1 }, 4, &frame3);
        write_rect(&mut expected_a, &Rect { x: 2, y: 2, w: 1, h: 1 }, 4, &frame3);
        assert_eq!(read_fd(&fd), expected_a, "A = frame 1 + both missed rects (full convergence)");
    }

    // PERF-DAMAGE regression (self-audit): a DROPPED frame (target buffer
    // still in flight) must still accumulate its damage — otherwise the next
    // writable buffer never repaints that region and pixels are lost.
    #[test]
    fn dropped_frame_damage_is_accumulated() {
        let _serial = fd_guard_lock().lock().unwrap();
        let _guard = FdCountGuard::new("dropped_frame_damage_is_accumulated");
        let mut cache = FrameCache::new(4, 4).unwrap();
        let full = fill_pattern(4, 4, 1);

        // Frame N → buffer A (full), App holds it (in flight).
        cache
            .push_damaged(4, 4, &[Rect { x: 0, y: 0, w: 4, h: 4 }], |dst, effective| {
                for r in effective {
                    write_rect(dst, r, 4, &full);
                }
            })
            .expect("frame N");

        // Frame N+1 → buffer B (full first write). Both A and B now in
        // flight: the App has not released either.
        let f2 = fill_pattern(4, 4, 2);
        cache
            .push_damaged(4, 4, &[Rect { x: 0, y: 0, w: 1, h: 1 }], |dst, effective| {
                assert_eq!(effective, &[Rect { x: 0, y: 0, w: 4, h: 4 }], "B's first write must be full-frame");
                for r in effective {
                    write_rect(dst, r, 4, &f2);
                }
            })
            .expect("frame N+1");

        // Frame N+2 → both buffers in flight → DROPPED. Its damage rect
        // (3,3,1,1) must still reach the next writable buffer's pending list.
        let f3 = fill_pattern(4, 4, 3);
        let dropped = cache.push_damaged(4, 4, &[Rect { x: 3, y: 3, w: 1, h: 1 }], |_, _| {
            panic!("dropped frame must not write");
        });
        assert!(dropped.is_none(), "both buffers in flight → frame dropped");

        // App releases A (frame N's buffer). Frame N+3 writes A again; its
        // effective rects must include the DROPPED frame's rect (3,3) — the
        // buggy order returned None before accumulating, losing the pixel.
        cache.on_release();
        let fd = cache
            .push_damaged(4, 4, &[Rect { x: 0, y: 0, w: 1, h: 1 }], |dst, effective| {
                let expected = vec![Rect { x: 0, y: 0, w: 1, h: 1 }, Rect { x: 3, y: 3, w: 1, h: 1 }];
                let mut eff = effective.to_vec();
                eff.sort_by_key(|r| (r.x, r.y));
                let mut exp = expected.clone();
                exp.sort_by_key(|r| (r.x, r.y));
                assert_eq!(eff, exp, "dropped frame's damage must be repainted");
                for r in effective {
                    write_rect(dst, r, 4, &f3);
                }
            })
            .expect("frame N+3");
        let a = read_fd(&fd);
        let mut expected_a = full.clone();
        write_rect(&mut expected_a, &Rect { x: 0, y: 0, w: 1, h: 1 }, 4, &f3);
        write_rect(&mut expected_a, &Rect { x: 3, y: 3, w: 1, h: 1 }, 4, &f3);
        assert_eq!(a, expected_a, "dropped frame's rect applied on the next writable turn");
    }
}

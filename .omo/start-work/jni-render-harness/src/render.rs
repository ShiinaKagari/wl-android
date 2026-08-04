//! Host-side harness for the jni_bridge.rs `render_frame` row-copy fix (TODO 10).
//!
//! The crate (`android-app/native`) cannot `cargo test` on host (build.rs
//! compiles C against the Android NDK), so the row-copy logic is extracted
//! into a pure helper (`copy_row_bgra`) and mirrored VERBATIM here for host
//! testing.

// VERBATIM copy of `copy_row_bgra` from
// /home/kagari/Projects/wl-android/android-app/native/src/jni_bridge.rs
fn copy_row_bgra(
    dst: &mut [u8],
    dst_stride_bytes: usize,
    src: &[u8],
    src_stride_bytes: usize,
    copy_w: usize,
    copy_h: usize,
) -> bool {
    let row_bytes = copy_w * 4;
    let mut truncated = false;
    for y in 0..copy_h {
        let src_row = y * src_stride_bytes;
        if src_row >= src.len() {
            truncated = true;
            continue;
        }
        let n = row_bytes.min(src.len() - src_row);
        if n < row_bytes {
            truncated = true;
        }
        if n == 0 {
            continue;
        }
        let dst_row = y * dst_stride_bytes;
        if dst_row >= dst.len() {
            truncated = true;
            continue;
        }
        let n = n.min(dst.len() - dst_row);
        unsafe {
            // `src` (SHM frame) and `dst` (window buffer) are distinct
            // allocations, so the regions never overlap.
            std::ptr::copy_nonoverlapping(
                src.as_ptr().add(src_row),
                dst.as_mut_ptr().add(dst_row),
                n,
            );
        }
    }
    truncated
}

// VERBATIM logic of the PRE-FIX per-pixel bpp==4 loop in
// `/home/kagari/Projects/wl-android/android-app/native/src/jni_bridge.rs`
// (lines 96-119 as of before TODO 10): BGRX -> RGBA reorder, NO clamping.
// Reading a fstat-truncated SHM frame (TODO 7) drives `src[src_off + 2]`
// out of bounds.
fn old_per_pixel_bgra(
    dst: &mut [u8],
    dst_stride_bytes: usize,
    src: &[u8],
    src_stride_bytes: usize,
    copy_w: usize,
    copy_h: usize,
) {
    let dst_bits = dst.as_mut_ptr();
    for y in 0..copy_h {
        for x in 0..copy_w {
            let src_off = y * src_stride_bytes + x * 4;
            let dst_off = y * dst_stride_bytes + x * 4;
            let b = src[src_off];       // BGRX: byte0=B
            let g = src[src_off + 1];   // byte1=G
            let r = src[src_off + 2];   // byte2=R
            unsafe {
                *dst_bits.add(dst_off) = r;        // RGBA: byte0=R
                *dst_bits.add(dst_off + 1) = g;    // byte1=G
                *dst_bits.add(dst_off + 2) = b;    // byte2=B
                *dst_bits.add(dst_off + 3) = 0xFF; // byte3=A
            }
        }
    }
}

// Per-pixel reference that produces the SAME bytes the row copy must produce:
// B,G,R,X written in order, no channel swap. This is the semantic the fast
// path replaces a loop with a memcpy for (the window is now BGRA_8888,
// byte-identical to the SHM frame; the OLD loop's RGBA reorder above was only
// correct for the pre-TODO-9 RGBA_8888 window).
fn per_pixel_passthrough(
    dst: &mut [u8],
    dst_stride_bytes: usize,
    src: &[u8],
    src_stride_bytes: usize,
    copy_w: usize,
    copy_h: usize,
) {
    let dst_bits = dst.as_mut_ptr();
    for y in 0..copy_h {
        for x in 0..copy_w {
            let src_off = y * src_stride_bytes + x * 4;
            let dst_off = y * dst_stride_bytes + x * 4;
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr().add(src_off), dst_bits.add(dst_off), 4);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Deterministic 4-byte-per-pixel source with distinct channel values.
    fn sample_src(src_stride: usize, w: usize, h: usize) -> Vec<u8> {
        let mut src = vec![0u8; src_stride * h];
        for y in 0..h {
            for x in 0..w {
                let o = y * src_stride + x * 4;
                src[o] = (y * w + x) as u8;                  // B
                src[o + 1] = ((y * w + x) * 3 + 1) as u8;    // G
                src[o + 2] = ((y * w + x) * 7 + 2) as u8;    // R
                src[o + 3] = 0xFF;                           // X/A
            }
        }
        src
    }

    #[test]
    fn row_copy_matches_per_pixel_loop() {
        let (w, h) = (4usize, 3usize);
        let src = sample_src(w * 4, w, h);
        let mut by_row = vec![0u8; w * 4 * h];
        let mut per_px = vec![0u8; w * 4 * h];
        copy_row_bgra(&mut by_row, w * 4, &src, w * 4, w, h);
        per_pixel_passthrough(&mut per_px, w * 4, &src, w * 4, w, h);
        assert_eq!(by_row, per_px, "row memcpy output must equal per-pixel output");
    }

    #[test]
    fn row_copy_clamps_truncated_source() {
        let (w, h) = (4usize, 3usize);
        let full = w * h * 4; // 48 bytes expected for a 4x3 BGRX frame
        // fstat-truncated frame (TODO 7): only 38 of 48 bytes survive.
        let src: Vec<u8> = (0..full - 10).map(|i| (i * 7) as u8).collect();
        // Regression guard: the PRE-FIX per-pixel loop panics reading OOB on
        // this truncated source.
        let mut dst_old = vec![0u8; w * 4 * h];
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            old_per_pixel_bgra(&mut dst_old, w * 4, &src, w * 4, w, h);
        }));
        assert!(
            panicked.is_err(),
            "old per-pixel loop must panic (OOB) on a truncated source"
        );
        // The NEW row copy clamps: rows 0-1 copied in full, row 2 copied up to
        // the source end (6 of 16 bytes), tail untouched, no panic.
        let mut dst = vec![0u8; w * 4 * h];
        let truncated = copy_row_bgra(&mut dst, w * 4, &src, w * 4, w, h);
        assert!(truncated, "must report truncation (caller warning path)");
        assert_eq!(&dst[..32], &src[..32], "rows 0-1 copied in full");
        assert_eq!(&dst[32..38], &src[32..38], "row 2 copied up to source end");
        assert_eq!(&dst[38..48], &[0u8; 10], "tail past source untouched");
    }

    #[test]
    fn row_copy_respects_strides() {
        let (w, h) = (4usize, 3usize);
        let src_stride = 6 * 4; // 24B rows (padded to 6 px)
        let dst_stride = 8 * 4; // 32B rows (padded to 8 px)
        let src = sample_src(src_stride, w, h);
        let mut dst = vec![0xFFu8; dst_stride * h]; // sentinel fill
        let truncated = copy_row_bgra(&mut dst, dst_stride, &src, src_stride, w, h);
        assert!(!truncated, "full source must not report truncation");
        for y in 0..h {
            let d = &dst[y * dst_stride..y * dst_stride + w * 4];
            let s = &src[y * src_stride..y * src_stride + w * 4];
            assert_eq!(d, s, "row {y}: copied bytes must match the src row");
        }
        // Row padding (x in 4..8 of each dst row) must stay sentinel.
        for y in 0..h {
            let pad = &dst[y * dst_stride + w * 4..(y + 1) * dst_stride];
            assert_eq!(pad, &[0xFFu8; 16], "row {y}: dst padding must be untouched");
        }
    }
}

//! doctor-harness — host-testable seams for `crates/wl-android/src/doctor.rs`
//! (TODO 32: mode / PERF-11 / fence / App SYNC_FD report).
//!
//! doctor is a CLI print; the only pure seam worth testing is the
//! `effective_mode` resolution (LAND_MODE → human-readable mode description).
//! The RED state below reproduces the PRE-TODO-32 behavior: doctor.rs had NO
//! mode-resolution function — the Mode line was hardcoded to
//! `"Mode: blit (Adreno 830 — direct dmabuf import unavailable)"` for every
//! LAND_MODE value. The tests below encode the new contract and therefore
//! FAIL against this stub (red). Once `effective_mode` lands in doctor.rs, the
//! VERBATIM copy is swapped in and the same tests pass (green).
//!
//! Contract (from TODO 32):
//!   - "auto"   → blit default path
//!   - "shm"    → debug CPU path
//!   - "direct" → documented-but-unavailable on Adreno 830
//!   - "blit"   → explicit blit path
//!   - unset (""), garbage, mixed case, whitespace → fall back to auto/blit.

/// GREEN state: VERBATIM copy of `effective_mode` from
/// /home/kagari/Projects/wl-android/crates/wl-android/src/doctor.rs
/// (TODO 32). If this diverges from the production function, this harness is
/// lying — keep in sync.
pub fn effective_mode(env: &str) -> &'static str {
    let mode = env.trim().to_ascii_lowercase();
    match mode.as_str() {
        "shm" => "shm (debug CPU path)",
        "direct" => "direct (unavailable on Adreno 830 — dmabuf import unsupported by host driver)",
        "blit" => "blit (explicit dmabuf blit path)",
        // "auto", "", or unknown → the default dmabuf blit path
        _ => "blit (default: dmabuf blit path)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_maps_to_blit_default() {
        assert_eq!(effective_mode("auto"), "blit (default: dmabuf blit path)");
    }

    #[test]
    fn unset_maps_to_auto_default() {
        // LAND_MODE unset → std::env::var Err → caller passes "".
        assert_eq!(effective_mode(""), "blit (default: dmabuf blit path)");
    }

    #[test]
    fn shm_maps_to_debug_cpu_path() {
        assert_eq!(effective_mode("shm"), "shm (debug CPU path)");
    }

    #[test]
    fn direct_maps_to_unavailable_note() {
        assert_eq!(
            effective_mode("direct"),
            "direct (unavailable on Adreno 830 — dmabuf import unsupported by host driver)"
        );
    }

    #[test]
    fn blit_maps_to_explicit() {
        assert_eq!(effective_mode("blit"), "blit (explicit dmabuf blit path)");
    }

    #[test]
    fn garbage_maps_to_auto_default() {
        // Malformed env must not crash or invent a mode.
        assert_eq!(effective_mode("banana"), "blit (default: dmabuf blit path)");
        assert_eq!(effective_mode("swapchain"), "blit (default: dmabuf blit path)");
    }

    #[test]
    fn case_and_whitespace_tolerated() {
        assert_eq!(effective_mode("  SHM "), "shm (debug CPU path)");
        assert_eq!(effective_mode("BLIT"), "blit (explicit dmabuf blit path)");
        assert_eq!(effective_mode("  "), "blit (default: dmabuf blit path)");
    }
}

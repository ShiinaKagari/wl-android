//! brdy-harness — host-testable pure seams for F-14 BRDY slot gating + the
//! LAND_MODE=shm retirement gate (TODO 31). Verbatim copies of the pure items
//! in crates/wl-android/src/state.rs; if they diverge, this harness is lying.
//!
//! What is tested here:
//!   - `SlotReadySet`: the per-slot ready set. mark_ready → is_ready,
//!     consume removes (double-consume is false), clear empties.
//!   - `slot_blittable`: a slot is blittable iff registered in the blit engine
//!     AND declared ready (BRDY or initial registration) AND not in use by an
//!     unacked frame.
//!   - `shm_path_enabled`: the SHM/CPU frame path survives only under
//!     LAND_MODE=shm; unset / "auto" / "blit" retire it.

// Verbatim state.rs items are consumed only by this crate's tests.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

// ============================================================================
// Verbatim pure items from state.rs (keep in sync!)
// ============================================================================

/// F-14: per-slot BRDY readiness set. A slot becomes blittable only when it is
/// in this set. Two producers: (1) TBUF registration (main.rs SlotRegistration
/// branch) — the App cannot BRDY a slot before it has presented a frame from
/// it, so the FIRST frame after registration is implicitly granted (deadlock
/// resolution); and (2) explicit BRDY (`handle_brdy`) after the App presents a
/// fence frame and releases the slot for reuse.
///
/// `blit_and_send_frame` consumes the flag on a successful blit; every reuse
/// requires a fresh BRDY (F-14). `free_slots_on_ack` does NOT re-add — an ack
/// returns the slot to the free pool, but blitting into it still waits for the
/// App's BRDY.
#[derive(Debug, Default)]
pub struct SlotReadySet {
    ready: HashSet<u32>,
}

impl SlotReadySet {
    pub fn mark_ready(&mut self, slot: u32) {
        self.ready.insert(slot);
    }

    /// Consume the ready flag for `slot`. Returns true iff the slot was ready
    /// (and is now consumed); a double-consume returns false.
    pub fn consume(&mut self, slot: u32) -> bool {
        self.ready.remove(&slot)
    }

    pub fn clear(&mut self) {
        self.ready.clear();
    }

    pub fn is_ready(&self, slot: u32) -> bool {
        self.ready.contains(&slot)
    }
}

/// F-14: a slot is blittable iff it is registered in the blit engine, the App
/// declared it ready (BRDY or initial registration), and no unacked frame is
/// in flight on it. The `in_use` check is implied by the ready-set consumption
/// (a consumed slot is not ready until the next BRDY) but kept as a defensive
/// double-check against an early BRDY-before-ack.
pub fn slot_blittable(
    slot: u32,
    slots: &HashMap<u32, u64>,
    ready: &SlotReadySet,
    in_use: &HashSet<u32>,
) -> bool {
    slots.contains_key(&slot) && ready.is_ready(slot) && !in_use.contains(&slot)
}

/// LAND_MODE=shm forces the legacy SHM/CPU frame path (frame_cache memfd
/// triple-buffer + pixel-fd frames). Any other value (unset / auto / blit)
/// retires it: SHM frames are logged and dropped, blit is the only frame
/// producer. Debug fallback only — KWin must be configured to produce dmabufs
/// (the doctor/deploy scripts set the right env).
pub fn shm_path_enabled() -> bool {
    std::env::var("LAND_MODE").map(|v| v == "shm").unwrap_or(false)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // F-14: mark_ready then consume removes the slot; is_ready flips false.
    #[test]
    fn slot_ready_consume_removes() {
        let mut set = SlotReadySet::default();
        set.mark_ready(1);
        assert!(set.is_ready(1), "marked slot must be ready");
        assert!(set.consume(1), "consume returns true for a ready slot");
        assert!(!set.is_ready(1), "consume removes the slot from the set");
    }

    // F-14: consuming a slot that was never marked ready is false, and a
    // double-consume (after the first removal) is also false — the flag is a
    // one-shot grant per BRDY.
    #[test]
    fn slot_ready_double_consume_is_false() {
        let mut set = SlotReadySet::default();
        assert!(!set.consume(3), "consume on a never-ready slot is false");
        set.mark_ready(3);
        assert!(set.consume(3));
        assert!(!set.consume(3), "double-consume is false (one-shot grant)");
    }

    // F-14 / C-02: AppLost clears the whole set — no slot survives a session
    // teardown as ready.
    #[test]
    fn slot_ready_clear_empties() {
        let mut set = SlotReadySet::default();
        set.mark_ready(0);
        set.mark_ready(2);
        set.clear();
        assert!(!set.is_ready(0));
        assert!(!set.is_ready(2));
        assert!(!set.consume(0), "clear empties the ready set");
    }

    // F-14: a slot is blittable only when all three hold — registered, ready,
    // and not in use. Missing registration / missing BRDY / in-flight frame
    // each veto the blit.
    #[test]
    fn slot_blittable_requires_registered_ready_and_free() {
        let mut slots = HashMap::new();
        slots.insert(0u32, 100u64);
        slots.insert(1u32, 200u64);
        let mut ready = SlotReadySet::default();
        ready.mark_ready(0);
        let empty: HashSet<u32> = HashSet::new();
        assert!(slot_blittable(0, &slots, &ready, &empty), "registered+ready+free");
        assert!(!slot_blittable(1, &slots, &ready, &empty), "registered but not ready");
        assert!(!slot_blittable(2, &slots, &ready, &empty), "not registered at all");
        let in_use: HashSet<u32> = [0].into_iter().collect();
        assert!(!slot_blittable(0, &slots, &ready, &in_use), "ready but in use (early BRDY)");
    }

    // F-14: the ready flag is consumed by the blit — a slot is NOT blittable
    // again until the App's next BRDY re-arms it.
    #[test]
    fn slot_blittable_after_consume_is_false() {
        let mut slots = HashMap::new();
        slots.insert(5u32, 1u64);
        let mut ready = SlotReadySet::default();
        ready.mark_ready(5);
        assert!(ready.consume(5), "blit consumed the slot's ready flag");
        assert!(!slot_blittable(5, &slots, &ready, &HashSet::new()), "consumed ⇒ not blittable");
    }

    // LAND_MODE gate: only the exact "shm" value keeps the SHM/CPU path alive.
    // env is process-global — this test is only deterministic under
    // --test-threads=1 (crate convention).
    #[test]
    fn shm_path_enabled_requires_exact_env() {
        unsafe { std::env::set_var("LAND_MODE", "shm") };
        assert!(shm_path_enabled(), "\"shm\" must enable the SHM path");
        unsafe { std::env::set_var("LAND_MODE", "auto") };
        assert!(!shm_path_enabled(), "\"auto\" must retire the SHM path");
        unsafe { std::env::set_var("LAND_MODE", "blit") };
        assert!(!shm_path_enabled(), "\"blit\" must retire the SHM path");
        unsafe { std::env::remove_var("LAND_MODE") };
        assert!(!shm_path_enabled(), "unset must retire the SHM path");
    }
}

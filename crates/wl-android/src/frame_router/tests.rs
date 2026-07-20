use super::*;

// ── F-04: MAX_IN_FLIGHT = 2 ──

#[test]
fn sends_up_to_max_in_flight() {
    let mut r = FrameRouter::new();
    r.handle(RouterEvent::AppConnected);

    // Commit 3 frames — first 2 should be enqueued, 3rd held
    let a1 = r.handle(RouterEvent::Commit { buffer_id: 1, has_fds: true, serial: 0 });
    assert_eq!(a1[0], RouterAction::EnqueueFrame { buffer_id: 1, serial: 1 });

    let a2 = r.handle(RouterEvent::Commit { buffer_id: 2, has_fds: true, serial: 0 });
    assert_eq!(a2[0], RouterAction::EnqueueFrame { buffer_id: 2, serial: 2 });

    let a3 = r.handle(RouterEvent::Commit { buffer_id: 3, has_fds: true, serial: 0 });
    assert!(a3.is_empty(), "third frame should be held (backpressure)");
    assert!(r.pending_frame.is_some());
}

// ── F-05: latest-wins ──

#[test]
fn latest_wins_replaces_pending() {
    let mut r = FrameRouter::new();
    r.handle(RouterEvent::AppConnected);

    // Fill in-flight window
    r.handle(RouterEvent::Commit { buffer_id: 1, has_fds: true, serial: 0 }); // s1, enqueued
    r.handle(RouterEvent::Commit { buffer_id: 2, has_fds: true, serial: 0 }); // s2, enqueued
    // Third frame: first pending — no callback needed (compositor hasn't waited yet)
    let a3 = r.handle(RouterEvent::Commit { buffer_id: 3, has_fds: true, serial: 0 });
    assert!(a3.is_empty(), "first held frame: no callback");
    // Fourth frame replaces pending → FireCallback for old pending
    let a4 = r.handle(RouterEvent::Commit { buffer_id: 4, has_fds: true, serial: 0 });
    assert_eq!(a4[0], RouterAction::FireCallback);
    assert!(r.pending_frame.is_some());
}

// ── F-06: headless drain ──

#[test]
fn headless_drain_discards_without_app() {
    let mut r = FrameRouter::new();
    // No app connected
    let actions = r.handle(RouterEvent::Commit { buffer_id: 1, has_fds: true, serial: 0 });
    assert!(actions.iter().any(|a| *a == RouterAction::FireCallback));
    assert!(r.in_flight.is_empty());
}

// ── F-11: cumulative ack ──

#[test]
fn cumulative_ack_releases_up_to_serial() {
    let mut r = FrameRouter::new();
    r.handle(RouterEvent::AppConnected);

    r.handle(RouterEvent::Commit { buffer_id: 1, has_fds: true, serial: 0 }); // s1
    r.handle(RouterEvent::Commit { buffer_id: 2, has_fds: true, serial: 0 }); // s2

    // Ack serial 2 → both should be released
    let actions = r.handle(RouterEvent::AppAck { serial: 2 });
    assert!(actions.iter().filter(|a| matches!(a, RouterAction::ReleaseBuffer { .. })).count() == 2);
    assert!(actions.iter().any(|a| *a == RouterAction::FireCallback));
    assert!(r.in_flight.is_empty());
}

// ── F-06: AppLost auto-ack ──

#[test]
fn app_lost_auto_acks_in_flight() {
    let mut r = FrameRouter::new();
    r.handle(RouterEvent::AppConnected);
    r.handle(RouterEvent::Commit { buffer_id: 1, has_fds: true, serial: 0 }); // s1
    r.handle(RouterEvent::Commit { buffer_id: 2, has_fds: true, serial: 0 }); // s2

    let actions = r.handle(RouterEvent::AppLost);
    assert!(actions.iter().filter(|a| matches!(a, RouterAction::ReleaseBuffer { .. })).count() == 2);
    assert!(actions.iter().filter(|a| **a == RouterAction::FireCallback).count() >= 2);
    assert!(r.in_flight.is_empty());
    assert!(!r.app_connected);
}

// ── serial monotonic ──

#[test]
fn serial_monotonic() {
    let mut r = FrameRouter::new();
    r.handle(RouterEvent::AppConnected);

    let a1 = r.handle(RouterEvent::Commit { buffer_id: 1, has_fds: true, serial: 0 });
    if let RouterAction::EnqueueFrame { serial: s1, .. } = a1[0] {
        assert_eq!(s1, 1);
    } else { panic!(); }

    let a2 = r.handle(RouterEvent::Commit { buffer_id: 1, has_fds: true, serial: 0 });
    if let RouterAction::EnqueueFrame { serial: s2, .. } = a2[0] {
        assert_eq!(s2, 2);
    } else { panic!(); }
}

// ── compositor lost → reconnects ──

#[test]
fn compositor_lost_clears_pending() {
    let mut r = FrameRouter::new();
    r.handle(RouterEvent::AppConnected);
    r.handle(RouterEvent::Commit { buffer_id: 1, has_fds: true, serial: 0 }); // s1
    r.handle(RouterEvent::Commit { buffer_id: 2, has_fds: true, serial: 0 }); // s2
    r.handle(RouterEvent::Commit { buffer_id: 3, has_fds: true, serial: 0 }); // held

    r.handle(RouterEvent::CompositorLost);
    assert!(r.pending_frame.is_none());
    assert!(!r.compositor_connected);
}

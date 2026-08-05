use std::collections::HashMap;

use tracing::debug;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterEvent {
    /// A surface was committed with buffer info.
    Commit { buffer_id: u32 },
    /// The App acknowledged frames up to `serial` (cumulative).
    AppAck { serial: u64 },
    /// App connected — enter active session.
    AppConnected,
    /// App disconnected — enter headless drain.
    AppLost,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RouterAction {
    /// Enqueue a frame for sending to App. `has_fds` indicates whether this
    /// is the first appearance of this buffer_id in the session (P-10).
    EnqueueFrame { buffer_id: u32, serial: u64, has_fds: bool },
    /// Release a wl_buffer.
    ReleaseBuffer { buffer_id: u32 },
    /// Fire a frame callback.
    FireCallback,
}

pub struct FrameRouter {
    serial: u64,
    pending_frame: Option<(u64, u32)>,  // (serial, buffer_id)
    in_flight: Vec<(u64, u32)>,  // (serial, buffer_id) of unacked frames (F-04)
    app_connected: bool,
    max_in_flight: usize,
    /// Registered buffer_ids in current session (P-10).
    /// Contains buffer_ids that have already been sent with fds.
    registered: HashMap<u32, ()>,
}

impl FrameRouter {
    pub fn new() -> Self {
        Self {
            serial: 0,
            pending_frame: None,
            in_flight: Vec::with_capacity(4),
            app_connected: false,
            max_in_flight: 2, // F-04
            registered: HashMap::new(),
        }
    }

    pub fn handle(&mut self, event: RouterEvent) -> Vec<RouterAction> {
        let mut actions = Vec::new();

        match event {
            RouterEvent::AppConnected => {
                self.app_connected = true;
                self.registered.clear();  // P-10: new session, clear buffer_id registry
                self.in_flight.clear();
                self.pending_frame = None;
                debug!("app connected, registry and in_flight cleared");
            }
            RouterEvent::AppLost => {
                self.app_connected = false;
                self.registered.clear();
                let entries: Vec<_> = self.in_flight.drain(..).collect();
                for &(_, buffer_id) in &entries {
                    actions.push(RouterAction::ReleaseBuffer { buffer_id });
                }
                if self.pending_frame.take().is_some() {
                    actions.push(RouterAction::FireCallback);
                }
                for _ in 0..entries.len() {
                    actions.push(RouterAction::FireCallback);
                }
                debug!("app lost, drained {} frames", entries.len());
            }
            RouterEvent::Commit { buffer_id } => {
                self.serial += 1;
                let serial = self.serial;

                // F-05: latest-wins — replace pending frame
                if let Some((_old_serial, _)) = self.pending_frame.take() {
                    debug!("latest-wins: replacing pending frame");
                    actions.push(RouterAction::FireCallback);
                }

                if self.app_connected {
                    let is_first = !self.registered.contains_key(&buffer_id);
                    if is_first {
                        self.registered.insert(buffer_id, ());
                    }

                    // Check in-flight window
                    if self.in_flight.len() < self.max_in_flight {
                        self.in_flight.push((serial, buffer_id));
                        actions.push(RouterAction::EnqueueFrame {
                            buffer_id,
                            serial,
                            has_fds: is_first,  // P-10: carry fds on first appearance
                        });
                    } else {
                        debug!(serial, "backpressure: holding frame (in_flight={})", self.in_flight.len());
                        self.pending_frame = Some((serial, buffer_id));
                    }
                } else {
                    debug!(serial, "headless drain: discarding frame");
                    actions.push(RouterAction::FireCallback);
                }
            }
            RouterEvent::AppAck { serial: ack_serial } => {
                let old_len = self.in_flight.len();
                self.in_flight.retain(|(s, _)| *s > ack_serial);
                let released = old_len - self.in_flight.len();
                for _ in 0..released {
                    actions.push(RouterAction::ReleaseBuffer { buffer_id: 0 });
                }

                // After ack, flush pending if room
                if self.in_flight.len() < self.max_in_flight
                    && let Some((_serial, buffer_id)) = self.pending_frame.take()
                {
                    self.serial += 1;
                    let new_serial = self.serial;
                    let is_first = !self.registered.contains_key(&buffer_id);
                    if is_first {
                        self.registered.insert(buffer_id, ());
                    }
                    self.in_flight.push((new_serial, buffer_id));
                    actions.push(RouterAction::EnqueueFrame {
                        buffer_id,
                        serial: new_serial,
                        has_fds: is_first,
                    });
                    debug!(new_serial, "unblocking pending frame after ack");
                }

                if released > 0 {
                    actions.push(RouterAction::FireCallback);
                }
            }
        }

        actions
    }
}

// ── Tests ──

#[cfg(test)]
mod tests;

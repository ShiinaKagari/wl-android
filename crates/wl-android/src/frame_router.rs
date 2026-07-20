use tracing::debug;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterEvent {
    /// A surface was committed with buffer info.
    Commit { buffer_id: u32, has_fds: bool, serial: u64 },
    /// The App acknowledged frames up to `serial` (cumulative).
    AppAck { serial: u64 },
    /// App connected — enter active session.
    AppConnected,
    /// App disconnected — enter headless drain.
    AppLost,
    /// Tick event for frame callback timing.
    Tick,
    /// Compositor disconnected.
    CompositorLost,
}

#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub enum RouterAction {
    /// Enqueue a frame for sending to App.
    EnqueueFrame { buffer_id: u32, serial: u64 },
    /// Release a wl_buffer.
    ReleaseBuffer { buffer_id: u32 },
    /// Fire a frame callback.
    FireCallback,
    /// Buffer destroyed — notify App (BGON).
    Gone { buffer_id: u32 },
    /// Drop a frame without sending (headless drain).
    DiscardFrame { serial: u64 },
}

pub struct FrameRouter {
    serial: u64,
    pending_frame: Option<u64>,  // serial of frame waiting to be sent (latest-wins, F-05)
    in_flight: Vec<u64>,         // serials of unacked frames (F-04)
    app_connected: bool,
    compositor_connected: bool,
    max_in_flight: usize,
}

impl FrameRouter {
    pub fn new() -> Self {
        Self {
            serial: 0,
            pending_frame: None,
            in_flight: Vec::with_capacity(4),
            app_connected: false,
            compositor_connected: false,
            max_in_flight: 2, // F-04
        }
    }

    pub fn handle(&mut self, event: RouterEvent) -> Vec<RouterAction> {
        let mut actions = Vec::new();

        match event {
            RouterEvent::AppConnected => {
                self.app_connected = true;
                debug!("app connected");
            }
            RouterEvent::AppLost => {
                self.app_connected = false;
                // F-06: headless drain — auto-ack all outstanding frames
                let serials: Vec<_> = self.in_flight.drain(..).collect();
                for _ in &serials {
                    actions.push(RouterAction::ReleaseBuffer { buffer_id: 0 });
                }
                if self.pending_frame.take().is_some() {
                    actions.push(RouterAction::FireCallback);
                }
                // Fire callbacks for each released frame to unblock compositor
                for _ in 0..serials.len() {
                    actions.push(RouterAction::FireCallback);
                }
                debug!("app lost, drained {} frames", serials.len());
            }
            RouterEvent::CompositorLost => {
                self.compositor_connected = false;
                self.pending_frame = None;
            }
            RouterEvent::Commit { buffer_id, has_fds: _, serial: _ } => {
                self.serial += 1;
                let serial = self.serial;
                self.compositor_connected = true;

                // F-05: latest-wins — replace pending frame
                if let Some(old_serial) = self.pending_frame.take() {
                    debug!(old_serial, "latest-wins: replacing pending frame");
                    // Old pending buffer is released immediately (never sent)
                    actions.push(RouterAction::FireCallback);
                }

                if self.app_connected {
                    // Check in-flight window
                    if self.in_flight.len() < self.max_in_flight {
                        // Send immediately
                        self.in_flight.push(serial);
                        actions.push(RouterAction::EnqueueFrame { buffer_id, serial });
                } else {
                    // F-04: backpressure — hold as pending, fire callback to not stall
                    debug!(serial, "backpressure: holding frame (in_flight={})", self.in_flight.len());
                    self.pending_frame = Some(serial);
                }
                } else {
                    // F-06: headless drain — discard, fire callback to unblock compositor
                    debug!(serial, "headless drain: discarding frame");
                    actions.push(RouterAction::FireCallback);
                }
            }
            RouterEvent::AppAck { serial: ack_serial } => {
                // F-11: cumulative ack
                let old_len = self.in_flight.len();
                self.in_flight.retain(|s| *s > ack_serial);
                let released = old_len - self.in_flight.len();
                for _ in 0..released {
                    actions.push(RouterAction::ReleaseBuffer { buffer_id: 0 });
                }

                // After ack, check if we can send pending
                if self.in_flight.len() < self.max_in_flight
                    && let Some(_serial) = self.pending_frame.take()
                {
                    self.serial += 1;
                    let new_serial = self.serial;
                    self.in_flight.push(new_serial);
                    actions.push(RouterAction::EnqueueFrame { buffer_id: 0, serial: new_serial });
                    debug!(new_serial, "unblocking pending frame after ack");
                }

                // Fire callback if a slot freed up
                if released > 0 {
                    actions.push(RouterAction::FireCallback);
                }
            }
            RouterEvent::Tick => {
                // Tick triggers frame callback if we have room
                if self.app_connected && self.in_flight.len() < self.max_in_flight {
                    if self.pending_frame.is_none() {
                        actions.push(RouterAction::FireCallback);
                    }
                } else if !self.app_connected {
                    // Headless drain: always fire callback
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

use std::collections::HashMap;

use smithay::backend::input::TouchSlot;
use smithay::input::touch::{DownEvent, MotionEvent, UpEvent};
use smithay::utils::{Point, Serial};
use tracing::debug;
use wl_android_common::proto::{TouchMessage, TOUCH_PHASE_CANCEL, TOUCH_PHASE_DOWN, TOUCH_PHASE_FRAME, TOUCH_PHASE_MOVE, TOUCH_PHASE_UP};

pub struct TouchInjector {
    next_serial: u32,
    logical_width: f64,
    logical_height: f64,
    active: HashMap<i32, ()>,
}

impl TouchInjector {
    pub fn new(logical_width: u32, logical_height: u32) -> Self {
        Self {
            next_serial: 1,
            logical_width: logical_width as f64,
            logical_height: logical_height as f64,
            active: HashMap::new(),
        }
    }

    pub fn set_logical_size(&mut self, w: u32, h: u32) {
        self.logical_width = w as f64;
        self.logical_height = h as f64;
    }

    fn slot_for(touch_id: i32) -> TouchSlot {
        // TouchSlot from an optional u32 id — use touch_id as the slot id
        TouchSlot::from(Some(touch_id as u32))
    }

    fn new_serial(&mut self) -> Serial {
        let s = Serial::from(self.next_serial);
        self.next_serial += 1;
        s
    }

    /// Handle a touch message.
    pub fn handle<D: smithay::input::SeatHandler + 'static>(
        &mut self,
        msg: &TouchMessage,
        touch: &smithay::input::touch::TouchHandle<D>,
        data: &mut D,
    ) {
        match msg.phase {
            TOUCH_PHASE_DOWN => {
                self.active.insert(msg.touch_id, ());
                let x = msg.x as f64 * self.logical_width;
                let y = msg.y as f64 * self.logical_height;
                let slot = Self::slot_for(msg.touch_id);
                debug!(touch_id = msg.touch_id, x, y, "touch down");

                touch.down(
                    data,
                    None,
                    &DownEvent {
                        slot,
                        location: Point::from((x, y)),
                        serial: self.new_serial(),
                        time: msg.time_ms,
                    },
                );
            }
            TOUCH_PHASE_MOVE => {
                if self.active.contains_key(&msg.touch_id) {
                    let x = msg.x as f64 * self.logical_width;
                    let y = msg.y as f64 * self.logical_height;
                    touch.motion(
                        data,
                        None,
                        &MotionEvent {
                            slot: Self::slot_for(msg.touch_id),
                            location: Point::from((x, y)),
                            time: msg.time_ms,
                        },
                    );
                }
            }
            TOUCH_PHASE_UP => {
                if self.active.remove(&msg.touch_id).is_some() {
                    debug!(touch_id = msg.touch_id, "touch up");
                    touch.up(
                        data,
                        &UpEvent {
                            slot: Self::slot_for(msg.touch_id),
                            serial: self.new_serial(),
                            time: msg.time_ms,
                        },
                    );
                }
            }
            TOUCH_PHASE_CANCEL => {
                touch.cancel(data);
                self.active.clear();
            }
            TOUCH_PHASE_FRAME => {
                touch.frame(data);
            }
            _ => {}
        }
    }
}

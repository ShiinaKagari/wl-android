use std::cell::Cell;
use std::time::{Duration, Instant};

thread_local! {
    static MOCK_NOW: Cell<Option<Instant>> = const { Cell::new(None) };
    static MOCK_TICK: Cell<Duration> = const { Cell::new(Duration::from_secs(0)) };
}

pub struct MockClock;

impl MockClock {
    pub fn set_now(now: Instant) {
        MOCK_NOW.with(|c| c.set(Some(now)));
    }

    pub fn now() -> Instant {
        MOCK_NOW.with(|c| c.get().unwrap_or_else(Instant::now))
    }

    pub fn advance(d: Duration) {
        let new = Self::now() + d;
        MOCK_NOW.with(|c| c.set(Some(new)));
    }

    pub fn clear() {
        MOCK_NOW.with(|c| c.set(None));
    }

    pub fn set_tick_interval(d: Duration) {
        MOCK_TICK.with(|c| c.set(d));
    }

    pub fn tick_interval() -> Duration {
        MOCK_TICK.with(|c| {
            let d = c.get();
            if d.is_zero() {
                Duration::from_secs_f64(1.0 / 144.0)
            } else {
                d
            }
        })
    }
}

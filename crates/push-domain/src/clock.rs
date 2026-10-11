//! Time source, injectable so tests control it.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeDelta, Utc};

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The real wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock that only moves when told to. For tests.
#[derive(Debug)]
pub struct ManualClock {
    now: Mutex<DateTime<Utc>>,
}

impl ManualClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(start),
        }
    }

    pub fn set(&self, to: DateTime<Utc>) {
        *self.now.lock().expect("clock lock poisoned") = to;
    }

    pub fn advance(&self, by: TimeDelta) {
        *self.now.lock().expect("clock lock poisoned") += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().expect("clock lock poisoned")
    }
}

impl<T: Clock + ?Sized> Clock for Arc<T> {
    fn now(&self) -> DateTime<Utc> {
        (**self).now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeDelta, TimeZone};

    #[test]
    fn manual_clock_returns_the_time_it_was_given_and_advances() {
        let start = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        let clock = ManualClock::new(start);
        assert_eq!(clock.now(), start);
        clock.advance(TimeDelta::minutes(5));
        assert_eq!(clock.now(), start + TimeDelta::minutes(5));
    }
}

//! Restart backoff for the backend: 1 s, 2 s, 4 s, … up to 60 s.

use std::time::Duration;

/// How long the backend must have run before its next failure starts the backoff over.
pub const RESET_AFTER: Duration = Duration::from_secs(60);

/// Doubling delays from `base` up to `cap`.
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    cap: Duration,
    next: Duration,
}

impl Backoff {
    /// 1 s base, 60 s cap.
    pub fn new() -> Self {
        Self::with(Duration::from_secs(1), Duration::from_secs(60))
    }

    pub fn with(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap,
            next: base.min(cap),
        }
    }

    /// The delay before the next restart; each call doubles the following one.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = delay.saturating_mul(2).min(self.cap);
        delay
    }

    /// Starts over at the base delay.
    pub fn reset(&mut self) {
        self.next = self.base.min(self.cap);
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seconds(backoff: &mut Backoff, count: usize) -> Vec<u64> {
        (0..count).map(|_| backoff.next_delay().as_secs()).collect()
    }

    #[test]
    fn delays_double_up_to_the_cap() {
        let mut backoff = Backoff::new();
        assert_eq!(seconds(&mut backoff, 9), [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[test]
    fn reset_starts_over() {
        let mut backoff = Backoff::new();
        seconds(&mut backoff, 4);
        backoff.reset();
        assert_eq!(seconds(&mut backoff, 3), [1, 2, 4]);
    }

    #[test]
    fn a_custom_base_and_cap() {
        let mut backoff = Backoff::with(Duration::from_millis(500), Duration::from_secs(3));
        let delays: Vec<_> = (0..5).map(|_| backoff.next_delay().as_millis()).collect();
        assert_eq!(delays, [500, 1000, 2000, 3000, 3000]);
    }
}

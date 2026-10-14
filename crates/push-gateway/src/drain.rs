//! Closing connections in batches when the gateway shuts down (spec §11), so the clients
//! it held do not all reconnect to the other gateways at the same instant.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Connections closed together.
const BATCH: usize = 100;
/// The pause between two batches.
const BATCH_INTERVAL: Duration = Duration::from_millis(50);
/// The last batch closes at most this long after shutdown began, well inside the 10 s
/// the gateway waits for its connections.
const SPREAD: Duration = Duration::from_secs(5);

/// Hands each closing connection its turn.
#[derive(Debug, Default)]
pub struct Drain {
    closing: AtomicUsize,
}

impl Drain {
    /// How long the next connection to close waits: 100 connections every 50 ms, at
    /// most 5 s.
    pub fn next_delay(&self) -> Duration {
        delay_for(self.closing.fetch_add(1, Ordering::Relaxed))
    }
}

fn delay_for(index: usize) -> Duration {
    let batch = u32::try_from(index / BATCH).unwrap_or(u32::MAX);
    BATCH_INTERVAL.saturating_mul(batch).min(SPREAD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connections_close_a_hundred_at_a_time_within_five_seconds() {
        let drain = Drain::default();
        let delays: Vec<Duration> = (0..250).map(|_| drain.next_delay()).collect();
        assert!(delays[..100].iter().all(|delay| delay.is_zero()));
        assert!(
            delays[100..200]
                .iter()
                .all(|delay| *delay == Duration::from_millis(50))
        );
        assert_eq!(delays[249], Duration::from_millis(100));
        assert_eq!(delay_for(1_000_000), Duration::from_secs(5));
        assert_eq!(delay_for(usize::MAX), Duration::from_secs(5));
    }
}

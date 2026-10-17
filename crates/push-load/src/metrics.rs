//! What a run measures: latency samples summarized by nearest rank, counters, the
//! sample messages in flight and the load generator's own scheduling lag.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

/// How every percentile in a report is defined.
pub const PERCENTILES: &str = "Nearest rank over every sample of the run, in milliseconds: pN is the smallest sample with at least N% of the samples at or below it; max is the largest sample. Connect latency runs from a connection's first attempt to Ready (including 4006 retries); reconnect latency from losing an open connection to Ready again; delivery from sending a sample message to its first Deliver on a receiving device; receipt from sending to the sender's Receipt covering it.";

/// How often the lag watcher expects to wake up.
pub const LAG_TICK: Duration = Duration::from_millis(100);
/// A p99 scheduling lag above this means the generator, not the target, limited the run.
pub const LAG_LIMIT: Duration = Duration::from_millis(250);

#[derive(Debug, Default)]
pub struct Samples(Mutex<Vec<u32>>);

impl Samples {
    pub fn record(&self, duration: Duration) {
        let millis = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX);
        self.0.lock().expect("samples lock").push(millis);
    }

    pub fn summary(&self) -> Summary {
        let mut values = self.0.lock().expect("samples lock").clone();
        values.sort_unstable();
        Summary::of(&values)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub count: usize,
    pub p50_ms: Option<u32>,
    pub p90_ms: Option<u32>,
    pub p99_ms: Option<u32>,
    pub max_ms: Option<u32>,
}

impl Summary {
    /// `sorted` ascending.
    pub fn of(sorted: &[u32]) -> Self {
        if sorted.is_empty() {
            return Self::default();
        }
        Self {
            count: sorted.len(),
            p50_ms: Some(nearest_rank(sorted, 50)),
            p90_ms: Some(nearest_rank(sorted, 90)),
            p99_ms: Some(nearest_rank(sorted, 99)),
            max_ms: sorted.last().copied(),
        }
    }
}

/// The smallest value with at least `percentile`% of `sorted` (ascending, non-empty) at or below it.
pub fn nearest_rank(sorted: &[u32], percentile: usize) -> u32 {
    let rank = (percentile * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

pub fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

pub fn read(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

#[derive(Debug, Default)]
pub struct Counters {
    pub ready_once: AtomicU64,
    pub attempt_errors: AtomicU64,
    pub connect_failures: AtomicU64,
    pub reconnect_failures: AtomicU64,
    pub auth_failures: AtomicU64,
    pub takeovers: AtomicU64,
    pub rate_limited: AtomicU64,
    pub pong_timeouts: AtomicU64,
    pub protocol_failures: AtomicU64,
    pub disconnects: AtomicU64,
    pub reconnects: AtomicU64,
    pub pings: AtomicU64,
    pub pongs: AtomicU64,
    pub auth_expiring: AtomicU64,
    pub error_frames: AtomicU64,
    active: AtomicI64,
    peak: AtomicI64,
}

impl Counters {
    pub fn opened(&self) {
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
    }

    pub fn closed(&self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn active(&self) -> i64 {
        self.active.load(Ordering::SeqCst)
    }

    pub fn peak(&self) -> i64 {
        self.peak.load(Ordering::SeqCst)
    }
}

/// WebSocket close codes and how often each came.
#[derive(Debug, Default)]
pub struct CloseCodes(Mutex<BTreeMap<u16, u64>>);

impl CloseCodes {
    pub fn record(&self, code: u16) {
        *self
            .0
            .lock()
            .expect("close codes lock")
            .entry(code)
            .or_default() += 1;
    }

    pub fn snapshot(&self) -> BTreeMap<String, u64> {
        self.0
            .lock()
            .expect("close codes lock")
            .iter()
            .map(|(code, count)| (code.to_string(), *count))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct MessageCounts {
    pub sent: u64,
    /// Answered by `SendAck`: durably stored.
    pub stored: u64,
    pub delivered: u64,
    pub receipted: u64,
    /// Stored but never delivered to a receiving device by the end of the run.
    pub lost: u64,
    /// Sent without a `SendAck` (the sender's connection ended first).
    pub unconfirmed: u64,
}

#[derive(Debug)]
struct Pending {
    sent_at: Instant,
    conversation: Vec<u8>,
    seq: Option<i64>,
    delivered: bool,
    receipted: bool,
}

/// The sample messages of a run, keyed by `client_msg_id`.
#[derive(Debug, Default)]
pub struct Messages {
    pending: Mutex<HashMap<Vec<u8>, Pending>>,
    pub delivery: Samples,
    pub receipt: Samples,
}

impl Messages {
    pub fn sent(&self, client_msg_id: Vec<u8>, conversation: Vec<u8>) {
        let pending = Pending {
            sent_at: Instant::now(),
            conversation,
            seq: None,
            delivered: false,
            receipted: false,
        };
        self.pending
            .lock()
            .expect("messages lock")
            .insert(client_msg_id, pending);
    }

    pub fn stored(&self, client_msg_id: &[u8], seq: i64) {
        if let Some(message) = self
            .pending
            .lock()
            .expect("messages lock")
            .get_mut(client_msg_id)
        {
            message.seq = Some(seq);
        }
    }

    /// A receiving device got it; only the first delivery counts.
    pub fn delivered(&self, client_msg_id: &[u8]) {
        if let Some(message) = self
            .pending
            .lock()
            .expect("messages lock")
            .get_mut(client_msg_id)
        {
            if !message.delivered {
                message.delivered = true;
                self.delivery.record(message.sent_at.elapsed());
            }
        }
    }

    /// The sender heard that the conversation is delivered up to `delivered_seq`.
    pub fn receipted(&self, conversation: &[u8], delivered_seq: i64) {
        for message in self.pending.lock().expect("messages lock").values_mut() {
            if !message.receipted
                && message.conversation == conversation
                && message.seq.is_some_and(|seq| seq <= delivered_seq)
            {
                message.receipted = true;
                self.receipt.record(message.sent_at.elapsed());
            }
        }
    }

    pub fn counts(&self) -> MessageCounts {
        let pending = self.pending.lock().expect("messages lock");
        let mut counts = MessageCounts::default();
        for message in pending.values() {
            counts.sent += 1;
            counts.stored += u64::from(message.seq.is_some());
            counts.delivered += u64::from(message.delivered);
            counts.receipted += u64::from(message.receipted);
            counts.lost += u64::from(message.seq.is_some() && !message.delivered);
            counts.unconfirmed += u64::from(message.seq.is_none());
        }
        counts
    }
}

/// Records how late a timer that should fire every [`LAG_TICK`] actually fires: a busy
/// generator falls behind and measures itself instead of the target.
pub async fn watch_lag(samples: Arc<Samples>, cancel: CancellationToken) {
    let mut expected = tokio::time::Instant::now() + LAG_TICK;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep_until(expected) => {}
        }
        let now = tokio::time::Instant::now();
        samples.record(now.saturating_duration_since(expected));
        expected = now + LAG_TICK;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_are_nearest_rank() {
        let values: Vec<u32> = (1..=100).collect();
        let summary = Summary::of(&values);
        assert_eq!(
            (
                summary.count,
                summary.p50_ms,
                summary.p90_ms,
                summary.p99_ms,
                summary.max_ms
            ),
            (100, Some(50), Some(90), Some(99), Some(100))
        );
        assert_eq!(nearest_rank(&[7], 99), 7);
        assert_eq!(nearest_rank(&[1, 2, 3], 50), 2);
        assert_eq!(Summary::of(&[]), Summary::default());
    }

    #[test]
    fn messages_count_stored_delivered_receipted_and_lost() {
        let messages = Messages::default();
        for id in [1u8, 2, 3, 4] {
            messages.sent(vec![id], b"conversation".to_vec());
        }
        messages.stored(&[1], 10);
        messages.stored(&[2], 11);
        messages.stored(&[3], 12);
        messages.delivered(&[1]);
        messages.delivered(&[1]);
        messages.delivered(&[2]);
        messages.delivered(&[9]);
        messages.receipted(b"conversation", 11);
        messages.receipted(b"other", 99);
        assert_eq!(
            messages.counts(),
            MessageCounts {
                sent: 4,
                stored: 3,
                delivered: 2,
                receipted: 2,
                lost: 1,
                unconfirmed: 1
            }
        );
        assert_eq!(messages.delivery.summary().count, 2);
        assert_eq!(messages.receipt.summary().count, 2);
    }

    #[test]
    fn the_peak_follows_open_connections() {
        let counters = Counters::default();
        counters.opened();
        counters.opened();
        counters.closed();
        counters.opened();
        assert_eq!((counters.active(), counters.peak()), (2, 2));
    }
}

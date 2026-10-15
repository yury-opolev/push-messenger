//! The connection's rules, free of I/O so they can be tested alone: what a close code
//! means (spec §7.4 and the owner's decisions), how long to wait before reconnecting, how
//! many frames the client may send (the gateway closes a connection that sends more than
//! 100 at once or 20 a second after that with 4004), and when to acknowledge deliveries.

use std::collections::BTreeMap;
use std::time::Duration;

use push_domain::ConversationId;
use push_proto::close;
use rand::Rng;
use tokio::time::Instant;

/// What to do after the connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AfterClose {
    /// 4001: the token is invalid or older than a password reset. Stop and ask the user
    /// to sign in again.
    SignOut,
    /// 4003: the device was revoked. Stop.
    DeviceRemoved,
    /// 4007: another connection of the device took over (the same app data opened
    /// elsewhere). Stop; the device is still signed in.
    OpenedElsewhere,
    /// 4002: the token expired. Refresh it and reconnect at once.
    Refresh,
    /// 4006: the device was registered moments ago and is not relayed yet. Reconnect
    /// soon; never a reason to sign out.
    NotReadyYet,
    /// Anything else — 4004, 4005, 1002, 1008, 1009, 1011, a network error, a missing
    /// close frame: reconnect with backoff.
    Reconnect,
}

/// The action for a close code; `None` when the connection ended without one.
pub(crate) fn after_close(code: Option<u16>) -> AfterClose {
    match code {
        Some(close::INVALID_IDENTITY) => AfterClose::SignOut,
        Some(close::DEVICE_REMOVED) => AfterClose::DeviceRemoved,
        Some(close::OPENED_ELSEWHERE) => AfterClose::OpenedElsewhere,
        Some(close::REAUTH_REQUIRED) => AfterClose::Refresh,
        Some(close::NOT_READY_YET) => AfterClose::NotReadyYet,
        _ => AfterClose::Reconnect,
    }
}

/// Exponential backoff with jitter (spec §7.3): the delay doubles from `base` to `cap`,
/// and each delay is drawn between half of it and all of it, so clients that dropped
/// together do not come back together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Backoff {
    pub base: Duration,
    pub cap: Duration,
}

impl Backoff {
    /// Reconnects after an error: 1 s up to 60 s.
    pub const RECONNECT: Self = Self {
        base: Duration::from_secs(1),
        cap: Duration::from_secs(60),
    };
    /// Reconnects while the device is not relayed yet (4006): 500 ms up to 5 s, so the
    /// worker's relay, about a second, is caught quickly while the gateway's limit of 30
    /// `Identify` a minute per device is never reached (at least 2.5 s apart once capped).
    pub const NOT_READY: Self = Self {
        base: Duration::from_millis(500),
        cap: Duration::from_secs(5),
    };
    /// Retries of an outbox row the server could not take (spec §8.4): 1 s up to 60 s.
    pub const OUTBOX: Self = Self {
        base: Duration::from_secs(1),
        cap: Duration::from_secs(60),
    };

    /// The delay before attempt `attempt` (0 for the first retry).
    pub fn delay(self, attempt: u32) -> Duration {
        let full = self.full(attempt);
        let half = full / 2;
        half + rand::rng().random_range(Duration::ZERO..=full - half)
    }

    /// The delay without jitter.
    fn full(self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX);
        self.base.saturating_mul(factor).min(self.cap)
    }
}

/// When to send the first `Ping` after `Ready`: somewhere within one heartbeat, so
/// clients that reconnected together do not ping together (spec §7.3).
pub(crate) fn first_ping(heartbeat: Duration) -> Duration {
    rand::rng().random_range(Duration::ZERO..=heartbeat)
}

/// The client's own frame budget, below the gateway's 100 at once and 20 a second: a
/// token bucket of 50 frames refilled at 10 a second. A frame that finds the bucket empty
/// waits rather than being dropped.
#[derive(Debug, Clone)]
pub(crate) struct FrameBudget {
    tokens: f64,
    updated: Instant,
}

impl FrameBudget {
    pub const BURST: f64 = 50.0;
    pub const PER_SECOND: f64 = 10.0;

    pub fn new(now: Instant) -> Self {
        Self {
            tokens: Self::BURST,
            updated: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * Self::PER_SECOND).min(Self::BURST);
        self.updated = now;
    }

    /// How long to wait before one more frame may go: zero if it may go now.
    pub fn wait(&mut self, now: Instant) -> Duration {
        self.refill(now);
        if self.tokens >= 1.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((1.0 - self.tokens) / Self::PER_SECOND)
        }
    }

    /// Counts one frame sent.
    pub fn spend(&mut self, now: Instant) {
        self.refill(now);
        self.tokens -= 1.0;
    }
}

/// Acknowledgements of deliveries, batched (spec §7.3, the frame budget): every stored
/// entry raises its conversation's position, and one `Ack` per conversation goes out once
/// deliveries pause for 50 ms, or 250 ms after the first one waiting. A backlog of a
/// thousand entries is acknowledged with a handful of frames, each naming only the
/// highest `seq`, which acknowledges everything before it.
#[derive(Debug, Default)]
pub(crate) struct Acks {
    waiting: BTreeMap<ConversationId, i64>,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Acks {
    pub const QUIET: Duration = Duration::from_millis(50);
    pub const MAX_DELAY: Duration = Duration::from_millis(250);

    /// `conversation` holds everything up to `seq` now.
    pub fn stored(&mut self, conversation: ConversationId, seq: i64, now: Instant) {
        let position = self.waiting.entry(conversation).or_insert(seq);
        *position = (*position).max(seq);
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    /// When the waiting acknowledgements are due, if any wait.
    pub fn due_at(&self) -> Option<Instant> {
        match (self.first, self.last) {
            (Some(first), Some(last)) => Some((last + Self::QUIET).min(first + Self::MAX_DELAY)),
            _ => None,
        }
    }

    /// The acknowledgements to send: the highest `seq` per conversation.
    pub fn take(&mut self) -> Vec<(ConversationId, i64)> {
        self.first = None;
        self.last = None;
        std::mem::take(&mut self.waiting).into_iter().collect()
    }

    /// Forgets what waits: the connection dropped, and the next one is acknowledged from
    /// the store's positions.
    pub fn clear(&mut self) {
        self.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_4001_signs_out_and_only_4003_and_4007_stop() {
        assert_eq!(after_close(Some(4001)), AfterClose::SignOut);
        assert_eq!(after_close(Some(4003)), AfterClose::DeviceRemoved);
        assert_eq!(after_close(Some(4007)), AfterClose::OpenedElsewhere);
        assert_eq!(after_close(Some(4002)), AfterClose::Refresh);
        assert_eq!(after_close(Some(4006)), AfterClose::NotReadyYet);
        for code in [4004, 4005, 1000, 1001, 1002, 1006, 1008, 1009, 1011] {
            assert_eq!(after_close(Some(code)), AfterClose::Reconnect, "{code}");
        }
        assert_eq!(after_close(None), AfterClose::Reconnect);
    }

    #[test]
    fn the_backoff_doubles_up_to_its_cap_with_jitter() {
        let backoff = Backoff::RECONNECT;
        for _ in 0..100 {
            let first = backoff.delay(0);
            assert!(first >= Duration::from_millis(500) && first <= Duration::from_secs(1));
            let fourth = backoff.delay(3);
            assert!(fourth >= Duration::from_secs(4) && fourth <= Duration::from_secs(8));
            let late = backoff.delay(40);
            assert!(late >= Duration::from_secs(30) && late <= Duration::from_secs(60));
        }
        let not_ready = Backoff::NOT_READY;
        assert!(not_ready.delay(0) <= Duration::from_millis(500));
        assert!(not_ready.delay(10) >= Duration::from_millis(2500));
        assert!(first_ping(Duration::from_secs(30)) <= Duration::from_secs(30));
    }

    #[test]
    fn the_frame_budget_stays_below_the_gateways() {
        let start = Instant::now();
        let mut budget = FrameBudget::new(start);
        for _ in 0..50 {
            assert_eq!(budget.wait(start), Duration::ZERO);
            budget.spend(start);
        }
        assert!(budget.wait(start) > Duration::ZERO, "the burst is used up");
        let later = start + Duration::from_millis(100);
        assert_eq!(budget.wait(later), Duration::ZERO, "10 a second refill");
    }

    #[test]
    fn a_backlog_is_acknowledged_with_one_ack_per_conversation() {
        let start = Instant::now();
        let mut acks = Acks::default();
        assert_eq!(acks.due_at(), None);
        let (a, b) = (ConversationId::new(), ConversationId::new());
        for seq in 1..=500 {
            acks.stored(a, seq, start);
        }
        acks.stored(b, 7, start);
        assert_eq!(acks.due_at(), Some(start + Acks::QUIET));
        let mut expected = vec![(a, 500), (b, 7)];
        expected.sort();
        assert_eq!(acks.take(), expected);
        assert_eq!(acks.due_at(), None);
    }

    #[test]
    fn a_steady_trickle_is_still_acknowledged_within_250_ms() {
        let start = Instant::now();
        let mut acks = Acks::default();
        let conversation = ConversationId::new();
        for step in 0..10 {
            acks.stored(
                conversation,
                step + 1,
                start + Duration::from_millis(40 * step as u64),
            );
        }
        assert_eq!(acks.due_at(), Some(start + Acks::MAX_DELAY));
    }
}

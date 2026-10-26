//! The per-connection pacing of `Activity` frames (docs/activity-presence.md): a token
//! bucket that never refuses, keeping the latest waiting activity per conversation, and
//! the record of activities still live for idle on disconnect.

use std::collections::HashMap;
use std::time::Duration;

use push_proto::v1::{Activity, ActivityState};
use tokio::time::Instant;

/// How long an activity lasts unless sent again (push_domain::ACTIVITY_TTL; the gateway
/// does not depend on push-domain).
pub const ACTIVITY_TTL: Duration = Duration::from_secs(30);

/// Fans out at most one activity per second per connection (a device has one live
/// connection: a takeover closes the older one), burst 2. Extra activities are not
/// refused: the latest per conversation waits for the next token, so the final state
/// always goes out. At most 16 conversations wait; a 17th is dropped.
///
/// An activity goes at once only when nothing waits: a waiting one of the same
/// conversation is replaced in place, so a newer activity never overtakes an older one
/// (which would leave the older as the final state), and other conversations queue
/// behind the waiting ones in order of first arrival.
#[derive(Debug)]
pub struct ActivityLimiter {
    tokens: f64,
    updated: Instant,
    /// Waiting activities, in order of first arrival, at most one per conversation.
    pending: Vec<(Vec<u8>, Activity)>,
    /// Conversations whose last accepted activity was not idle, and when it expires.
    live: HashMap<Vec<u8>, Instant>,
}

impl ActivityLimiter {
    /// Activities a connection may send at once.
    pub const BURST: f64 = 2.0;
    /// Activities a second a connection may keep sending.
    pub const PER_SECOND: f64 = 1.0;
    /// Conversations that may wait for a token at once.
    pub const MAX_PENDING: usize = 16;
    /// Conversations remembered for idle on disconnect. Only conversations message-svc
    /// accepted an activity for get in, at most one a second; past this the expired ones
    /// are forgotten first, then the one expiring soonest.
    pub const MAX_LIVE: usize = 256;

    pub fn new(now: Instant) -> Self {
        Self {
            tokens: Self::BURST,
            updated: now,
            pending: Vec::new(),
            live: HashMap::new(),
        }
    }

    /// An activity from the client: `Some` to send now, or `None` (queued, replacing a
    /// waiting one of the same conversation in place, or dropped).
    pub fn offer(&mut self, activity: Activity, now: Instant) -> Option<Activity> {
        self.refill(now);
        if let Some((_, waiting)) = self
            .pending
            .iter_mut()
            .find(|(conversation, _)| *conversation == activity.conversation_id)
        {
            *waiting = activity;
            return None;
        }
        if self.pending.is_empty() && self.free() {
            self.tokens -= 1.0;
            return Some(activity);
        }
        if self.pending.len() < Self::MAX_PENDING {
            self.pending
                .push((activity.conversation_id.clone(), activity));
        }
        None
    }

    /// When the next waiting activity may go, if any.
    pub fn next_due(&self) -> Option<Instant> {
        if self.pending.is_empty() {
            return None;
        }
        let missing = (1.0 - self.tokens).max(0.0);
        Some(self.updated + Duration::from_secs_f64(missing / Self::PER_SECOND))
    }

    /// The waiting activity to send now (FIFO by first arrival), if a token is free.
    pub fn take_due(&mut self, now: Instant) -> Option<Activity> {
        self.refill(now);
        if self.pending.is_empty() || !self.free() {
            return None;
        }
        self.tokens -= 1.0;
        Some(self.pending.remove(0).1)
    }

    /// Records an activity message-svc accepted, for idle on disconnect: non-idle → live
    /// until now + TTL; idle → forgotten.
    pub fn sent(&mut self, activity: &Activity, now: Instant) {
        if activity.state() == ActivityState::Idle {
            self.live.remove(&activity.conversation_id);
            return;
        }
        if !self.live.contains_key(&activity.conversation_id) && self.live.len() >= Self::MAX_LIVE {
            self.live.retain(|_, expires| *expires > now);
            if self.live.len() >= Self::MAX_LIVE {
                let soonest = self
                    .live
                    .iter()
                    .min_by_key(|(_, expires)| **expires)
                    .map(|(conversation, _)| conversation.clone());
                if let Some(soonest) = soonest {
                    self.live.remove(&soonest);
                }
            }
        }
        self.live
            .insert(activity.conversation_id.clone(), now + ACTIVITY_TTL);
    }

    /// Conversations whose last accepted activity is unexpired and not idle.
    pub fn still_live(&self, now: Instant) -> Vec<Vec<u8>> {
        self.live
            .iter()
            .filter(|(_, expires)| **expires > now)
            .map(|(conversation, _)| conversation.clone())
            .collect()
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * Self::PER_SECOND).min(Self::BURST);
        self.updated = self.updated.max(now);
    }

    /// Whether a token is free. The slack absorbs rounding: a timer set by
    /// [`Self::next_due`] must find the token it waited for.
    fn free(&self) -> bool {
        self.tokens >= 1.0 - 1e-6
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn activity(conversation: u8, state: ActivityState, label: &str) -> Activity {
        Activity {
            conversation_id: vec![conversation; 16],
            state: state.into(),
            label: (!label.is_empty()).then(|| label.to_owned()),
        }
    }

    fn typing(conversation: u8, label: &str) -> Activity {
        activity(conversation, ActivityState::Typing, label)
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[test]
    fn two_go_at_once_then_one_per_second() {
        let start = Instant::now();
        let mut limiter = ActivityLimiter::new(start);
        assert!(limiter.offer(typing(1, "a"), start).is_some());
        assert!(limiter.offer(typing(2, "b"), start).is_some());
        assert_eq!(limiter.next_due(), None);
        assert_eq!(limiter.offer(typing(3, "c"), start), None);
        let due = limiter.next_due().expect("one waits");
        assert!(due >= start + ms(999) && due <= start + ms(1001), "{due:?}");
        assert_eq!(limiter.take_due(start + ms(500)), None, "no token yet");
        assert_eq!(limiter.take_due(due), Some(typing(3, "c")));
        assert_eq!(limiter.next_due(), None);
        // The bucket refills to two, not more.
        let later = due + Duration::from_secs(10);
        assert!(limiter.offer(typing(4, ""), later).is_some());
        assert!(limiter.offer(typing(5, ""), later).is_some());
        assert_eq!(limiter.offer(typing(6, ""), later), None);
    }

    #[test]
    fn the_latest_waiting_activity_per_conversation_wins() {
        let start = Instant::now();
        let mut limiter = ActivityLimiter::new(start);
        limiter.offer(typing(9, ""), start);
        limiter.offer(typing(9, ""), start);
        assert_eq!(limiter.offer(typing(1, "first"), start), None);
        assert_eq!(limiter.offer(typing(2, "other"), start), None);
        let working = activity(1, ActivityState::Working, "second");
        assert_eq!(limiter.offer(working.clone(), start), None);
        // Conversation 1 keeps its place ahead of 2, with its latest activity.
        let due = limiter.next_due().unwrap();
        assert_eq!(limiter.take_due(due), Some(working));
        let due = limiter.next_due().unwrap();
        assert_eq!(limiter.take_due(due), Some(typing(2, "other")));
        assert_eq!(limiter.next_due(), None);
    }

    #[test]
    fn a_newer_activity_never_overtakes_a_waiting_one_of_the_same_conversation() {
        let start = Instant::now();
        let mut limiter = ActivityLimiter::new(start);
        limiter.offer(typing(1, "one"), start);
        limiter.offer(typing(1, "two"), start);
        assert_eq!(limiter.offer(typing(1, "three"), start), None);
        // A token is free again, but "three" still waits: the newest must go through the
        // queue, replacing it, so it is the last one out.
        let later = start + Duration::from_secs(5);
        let idle = activity(1, ActivityState::Idle, "");
        assert_eq!(limiter.offer(idle.clone(), later), None);
        assert_eq!(limiter.take_due(later), Some(idle));
        assert_eq!(limiter.take_due(later), None);
    }

    #[test]
    fn at_most_16_conversations_wait() {
        let start = Instant::now();
        let mut limiter = ActivityLimiter::new(start);
        limiter.offer(typing(100, ""), start);
        limiter.offer(typing(101, ""), start);
        for conversation in 0..17 {
            assert_eq!(limiter.offer(typing(conversation, ""), start), None);
        }
        let mut out = Vec::new();
        let mut now = start;
        while let Some(due) = limiter.next_due() {
            now = now.max(due);
            out.extend(limiter.take_due(now));
        }
        assert_eq!(out, (0..16).map(|c| typing(c, "")).collect::<Vec<_>>());
        // A waiting conversation is still replaced while the queue is full.
        let mut limiter = ActivityLimiter::new(start);
        limiter.offer(typing(100, ""), start);
        limiter.offer(typing(101, ""), start);
        for conversation in 0..16 {
            limiter.offer(typing(conversation, ""), start);
        }
        limiter.offer(typing(0, "newer"), start);
        let due = limiter.next_due().unwrap();
        assert_eq!(limiter.take_due(due), Some(typing(0, "newer")));
    }

    #[test]
    fn still_live_forgets_idle_and_expired() {
        let start = Instant::now();
        let mut limiter = ActivityLimiter::new(start);
        limiter.sent(&typing(1, ""), start);
        limiter.sent(
            &activity(2, ActivityState::Working, "x"),
            start + ms(10_000),
        );
        limiter.sent(&activity(3, ActivityState::Waiting, ""), start);
        limiter.sent(&activity(3, ActivityState::Idle, ""), start + ms(1));
        let mut live = limiter.still_live(start + ms(20_000));
        live.sort();
        assert_eq!(live, vec![vec![1; 16], vec![2; 16]]);
        // Conversation 1 expires 30 s after it was sent; 2 lasts 10 s longer.
        assert_eq!(limiter.still_live(start + ms(30_000)), vec![vec![2; 16]]);
        assert!(limiter.still_live(start + ms(40_000)).is_empty());
    }
}

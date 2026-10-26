//! Events for the transport layer.
//!
//! After a commit, the service publishes typed events: which users have new log entries,
//! whose delivery or read position moved, whose contacts changed, and which devices must
//! disconnect. Events are best effort and ephemeral. They carry no message bodies, and
//! losing one only delays a client, because everything they announce is durable and can
//! be read back:
//!
//! - new entries through `sync`;
//! - contact changes through `list_contacts`;
//! - receipt positions through `list_conversations` called without `changed_since`.
//!   `changed_since` reflects content changes only (a conversation created or an entry
//!   appended). Receipt positions do not move `updated_at`, so the busy conversation row
//!   is not written on every ack and read. A reconnecting client refreshes its ticks with
//!   one listing without `changed_since`;
//! - a kick through `identify`, which refuses a revoked device, and through the access
//!   token, which a password reset stops renewing: a lost kick leaves a socket open until
//!   the gateway next checks the device or the token (Plan 8).
//!
//! Activity and presence events are the exception: they announce nothing durable. A
//! missed activity is simply gone, which costs little because it expires after
//! [`ACTIVITY_TTL`](crate::ACTIVITY_TTL) anyway; presence is read back with the presence
//! batch read ([`Messaging::presence_viewable`](crate::Messaging::presence_viewable) and
//! the transport's presence states).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};

use crate::activity::ActivityState;
use crate::ids::{ConversationId, DeviceId, UserId};
use crate::model::ContactState;
use crate::presence::PresenceView;

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// New log entries exist for these users; their devices should sync.
    EntriesAvailable { users: Vec<UserId> },
    /// Delivery or read positions changed in a conversation; push a receipt to `to`.
    Receipt {
        conversation: ConversationId,
        to: Vec<UserId>,
        receipt: ReceiptUpdate,
    },
    /// A contact relationship changed as seen by `user`. `None` means no relationship.
    ContactChanged {
        user: UserId,
        peer: UserId,
        state: Option<ContactState>,
    },
    /// `user`'s own read position moved, by reading or by sending; their other devices
    /// update their unread counts. Published to `user` only.
    ReadPositionChanged {
        user: UserId,
        conversation: ConversationId,
        read_seq: i64,
    },
    /// These devices must be disconnected at once (spec §5.3, §5.4). Published to the
    /// devices, not to their users. Repeating a kick is harmless.
    Kick {
        /// Sorted.
        devices: Vec<DeviceId>,
        reason: KickReason,
    },
    /// `user`'s activity in `conversation`; push to `to` (sorted). `ttl` is zero for idle.
    Activity {
        to: Vec<UserId>,
        conversation: ConversationId,
        user: UserId,
        state: ActivityState,
        label: Option<String>,
        ttl: TimeDelta,
    },
    /// `user`'s presence as `to` (sorted) may see it.
    Presence {
        to: Vec<UserId>,
        user: UserId,
        view: PresenceView,
    },
}

/// Why devices are kicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KickReason {
    /// The device was revoked: it must stop.
    DeviceRevoked,
    /// The user's password was reset at `reset_at`: every refresh token is revoked, so the
    /// device must sign in again. A connection whose access token was issued after the
    /// second of `reset_at` signed in after the reset and is not closed.
    PasswordReset { reset_at: DateTime<Utc> },
}

/// The positions a receipt reports.
///
/// Match this enum with a wildcard arm: later versions may report positions of other
/// kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReceiptUpdate {
    /// A direct conversation: the positions of the member whose position moved.
    /// `delivered_seq` is never below `read_seq`: a message read is a message delivered.
    Direct {
        user_id: UserId,
        delivered_seq: i64,
        read_seq: i64,
    },
    /// A group: every live message up to `all_read_seq` has been read, and every one up to
    /// `all_delivered_seq` delivered, by every current member other than its sender. Never
    /// per-member positions. `all_delivered_seq` is never below `all_read_seq`.
    ///
    /// The transport sends at most one of these per second per conversation, keeping the
    /// latest (spec §4.5); the domain publishes one for every change.
    Group {
        all_delivered_seq: i64,
        all_read_seq: i64,
    },
}

#[async_trait]
pub trait Notifier: Send + Sync {
    /// Called only after the triggering transaction committed, except for an
    /// `Event::Presence` from an announcement or from switching sharing on, which is
    /// published while the publishing transaction still holds the user's lock (see
    /// `Messaging::announce_presence`). The `Unknown` of a hide or a block comes after its
    /// commit. Best effort: it must not fail the operation that triggered it.
    async fn publish(&self, events: &[Event]);

    /// Kicks devices (a revocation or a password reset), after the commit. Unlike
    /// [`Self::publish`] a failure is reported: the relay that asked for the kick fails
    /// and the worker retries it, so a lost kick cannot leave a signed-out session open
    /// until its token expires. A repeated kick is harmless.
    async fn kick(&self, events: &[Event]) -> Result<(), String> {
        self.publish(events).await;
        Ok(())
    }
}

/// Discards events.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopNotifier;

#[async_trait]
impl Notifier for NoopNotifier {
    async fn publish(&self, _events: &[Event]) {}
}

/// Records events. For tests; [`Self::fail_kicks`] makes its kicks fail.
#[derive(Debug, Default)]
pub struct RecordingNotifier {
    events: Mutex<Vec<Event>>,
    failing_kicks: std::sync::atomic::AtomicBool,
}

impl RecordingNotifier {
    /// Returns and clears every event published so far, in publication order.
    pub fn take(&self) -> Vec<Event> {
        std::mem::take(&mut *self.events.lock().expect("notifier lock poisoned"))
    }

    /// Makes kicks fail (and record nothing) until called again with `false`.
    pub fn fail_kicks(&self, failing: bool) {
        self.failing_kicks
            .store(failing, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl Notifier for RecordingNotifier {
    async fn publish(&self, events: &[Event]) {
        self.events
            .lock()
            .expect("notifier lock poisoned")
            .extend_from_slice(events);
    }

    async fn kick(&self, events: &[Event]) -> Result<(), String> {
        if self.failing_kicks.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("kicks are failing".to_owned());
        }
        self.publish(events).await;
        Ok(())
    }
}

#[async_trait]
impl<T: Notifier + ?Sized> Notifier for Arc<T> {
    async fn publish(&self, events: &[Event]) {
        (**self).publish(events).await;
    }

    async fn kick(&self, events: &[Event]) -> Result<(), String> {
        (**self).kick(events).await
    }
}

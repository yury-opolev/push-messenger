use chrono::TimeDelta;

use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
use crate::ids::{DeviceId, UserId};
use crate::model::Member;
use crate::notify::{Event, Notifier};
use crate::store::{Store, Tx};

/// Server settings for changing messages after they are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// How long after sending its sender may edit a message.
    pub edit_window: TimeDelta,
    /// How long after sending its sender may delete a message.
    pub delete_window: TimeDelta,
}

impl Default for Policy {
    /// 48 hours for both.
    fn default() -> Self {
        Self {
            edit_window: TimeDelta::hours(48),
            delete_window: TimeDelta::hours(48),
        }
    }
}

/// The messaging service. Every operation runs in one storage transaction.
///
/// A transaction dropped without `commit` is rolled back, so an early return through `?`
/// never leaves partial changes behind.
#[derive(Debug)]
pub struct Messaging<S, C, N> {
    pub(crate) store: S,
    pub(crate) clock: C,
    pub(crate) notifier: N,
    pub(crate) policy: Policy,
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// A service with the default [`Policy`].
    pub fn new(store: S, clock: C, notifier: N) -> Self {
        Self::with_policy(store, clock, notifier, Policy::default())
    }

    pub fn with_policy(store: S, clock: C, notifier: N, policy: Policy) -> Self {
        Self {
            store,
            clock,
            notifier,
            policy,
        }
    }

    /// Records a user whose email is verified. Idempotent.
    pub async fn register_user(&self, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        tx.insert_user(user).await?;
        tx.commit().await
    }

    /// Records a newly registered device. Idempotent.
    ///
    /// The device starts up to date in every conversation of its user, including ones the
    /// user has left (up to where they left); older messages are loaded through history.
    pub async fn register_device(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        if !tx.user_exists(user).await? {
            return Err(DomainError::UnknownUser);
        }
        if !tx.insert_device(device, user).await? {
            return Err(DomainError::UnknownDevice);
        }
        for (conversation, member) in tx.memberships(user).await? {
            let position = member.visible_upper(conversation.last_seq);
            tx.create_cursor_if_absent(device, conversation.id, position)
                .await?;
        }
        tx.commit().await
    }

    pub async fn revoke_device(&self, device: DeviceId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        tx.revoke_device(device).await?;
        tx.commit().await
    }

    /// Publishes events after a commit. Publishes nothing when there is nothing to say.
    pub(crate) async fn publish(&self, events: Vec<Event>) {
        if !events.is_empty() {
            self.notifier.publish(&events).await;
        }
    }

    /// Confirms that a connecting device is active and belongs to the user in its token.
    pub async fn identify(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        let owner = require_device(&mut tx, device).await?;
        tx.rollback().await?;
        if owner == user {
            Ok(())
        } else {
            Err(DomainError::UnknownDevice)
        }
    }
}

/// The owner of an active device, or `UnknownDevice`.
pub(crate) async fn require_device<T: Tx>(tx: &mut T, device: DeviceId) -> DomainResult<UserId> {
    tx.device_owner(device)
        .await?
        .ok_or(DomainError::UnknownDevice)
}

/// The ids of the current members, sorted so events are deterministic.
pub(crate) fn current_member_ids(members: &[Member]) -> Vec<UserId> {
    let mut users: Vec<UserId> = members
        .iter()
        .filter(|member| member.is_current())
        .map(|member| member.user_id)
        .collect();
    users.sort();
    users
}

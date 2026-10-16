use chrono::{DateTime, TimeDelta, Utc};

use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
use crate::ids::{DeviceId, UserId};
use crate::model::Member;
use crate::notify::{Event, KickReason, Notifier};
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
        tx.lock_users(&[user]).await?;
        if !tx.insert_device(device, user).await? {
            return Err(DomainError::UnknownDevice);
        }
        create_cursors(&mut tx, device, user).await?;
        tx.commit().await
    }

    pub async fn revoke_device(&self, device: DeviceId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        tx.revoke_device(device).await?;
        tx.commit().await
    }

    /// auth-svc registered `device` for `user` (`device.registered`, relayed by the
    /// worker). Idempotent, and correct in any order with [`Self::device_revoked`] and
    /// [`Self::register_user`]: the user is recorded too, since auth-svc registers devices
    /// only for verified users, and a device already revoked stays revoked. A device id
    /// that belongs to another user is `UnknownDevice`.
    pub async fn device_registered(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        tx.insert_user(user).await?;
        tx.lock_users(&[user]).await?;
        if tx.insert_device(device, user).await? {
            create_cursors(&mut tx, device, user).await?;
        } else if tx.device_user(device).await? != Some(user) {
            return Err(DomainError::UnknownDevice);
        }
        tx.commit().await
    }

    /// auth-svc revoked `device` of `user` (`device.revoked`, relayed by the worker).
    /// Idempotent and correct in any order: a revocation that arrives before the
    /// registration records the device as revoked, so the registration cannot bring it
    /// back. A device id that belongs to another user is `UnknownDevice`. After the commit
    /// the device is kicked, every time: a repeated kick is harmless.
    pub async fn device_revoked(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        tx.insert_user(user).await?;
        if !tx.insert_device(device, user).await? && tx.device_user(device).await? != Some(user) {
            return Err(DomainError::UnknownDevice);
        }
        tx.revoke_device(device).await?;
        tx.commit().await?;
        self.publish(vec![Event::Kick {
            devices: vec![device],
            reason: KickReason::DeviceRevoked,
        }])
        .await;
        Ok(())
    }

    /// auth-svc reset `user`'s password at `reset_at` (`user.password_reset`, relayed by
    /// the worker): every active device of the user is kicked, to sign in again with the
    /// new password, and [`Self::identify`] refuses access tokens issued no later than that
    /// second, so a token that outlives the reset (up to 30 minutes) or a lost kick cannot
    /// keep a device signed in. `reset_at` is auth-svc's time, the clock of the tokens'
    /// `iat`; an event from before auth-svc sent it (`None`) takes message-svc's clock.
    ///
    /// The devices stay registered. Idempotent and correct in any order: the user is
    /// recorded if message-svc has not heard of them yet, the latest reset is kept, so a
    /// redelivered or older event moves nothing (except one without `reset_at`, which
    /// takes message-svc's clock again on every delivery), and the kick carries the latest reset,
    /// so it spares a session that signed in after it.
    pub async fn password_reset(
        &self,
        user: UserId,
        reset_at: Option<DateTime<Utc>>,
    ) -> DomainResult<()> {
        let reset_at = reset_at.unwrap_or_else(|| self.clock.now());
        let mut tx = self.store.begin().await?;
        tx.insert_user(user).await?;
        let reset_at = tx.record_password_reset(user, reset_at).await?;
        let mut devices = tx.devices_of_user(user).await?;
        tx.commit().await?;
        if !devices.is_empty() {
            devices.sort();
            self.publish(vec![Event::Kick {
                devices,
                reason: KickReason::PasswordReset { reset_at },
            }])
            .await;
        }
        Ok(())
    }

    /// Publishes events after a commit. Publishes nothing when there is nothing to say.
    pub(crate) async fn publish(&self, events: Vec<Event>) {
        if !events.is_empty() {
            self.notifier.publish(&events).await;
        }
    }

    /// Confirms that a connecting device is active and belongs to the user in its token,
    /// a token issued at `issued_at` (its `iat`, whole seconds). A device never heard of is
    /// `DeviceNotKnownYet` (retryable): auth-svc registered it and the worker has not
    /// relayed it yet. A revoked device, or one of another user, is `UnknownDevice`. A
    /// token issued no later than the second of the user's last password reset is
    /// `PasswordChanged`: within that second the order is unknown, and refusing a token
    /// minted just after the reset only asks for one more sign-in.
    pub async fn identify(
        &self,
        device: DeviceId,
        user: UserId,
        issued_at: DateTime<Utc>,
    ) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        // One statement: reading the owner and the revocation separately could straddle
        // the worker's commit and refuse a device that is merely not relayed yet.
        let state = tx.device_state(device).await?;
        tx.rollback().await?;
        let Some(state) = state else {
            return Err(DomainError::DeviceNotKnownYet);
        };
        if state.user != user || !state.active {
            return Err(DomainError::UnknownDevice);
        }
        match state.password_reset_at {
            Some(reset) if issued_at.timestamp() <= reset.timestamp() => {
                Err(DomainError::PasswordChanged)
            }
            _ => Ok(()),
        }
    }
}

/// Starts a new device up to date in every conversation of its user, including ones the
/// user has left (up to where they left). The caller holds the user's lock
/// ([`Tx::lock_users`]), so no membership of the user is being created meanwhile: every
/// membership gets a position, here or by the join.
async fn create_cursors<T: Tx>(tx: &mut T, device: DeviceId, user: UserId) -> DomainResult<()> {
    for (conversation, member) in tx.memberships(user).await? {
        let position = member.visible_upper(conversation.last_seq);
        tx.create_cursor_if_absent(device, conversation.id, position)
            .await?;
    }
    Ok(())
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

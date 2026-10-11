use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
use crate::ids::{DeviceId, UserId};
use crate::notify::Notifier;
use crate::store::{Store, Tx};

/// The messaging service. Every operation runs in one storage transaction.
///
/// A transaction dropped without `commit` is rolled back, so an early return through `?`
/// never leaves partial changes behind.
#[derive(Debug)]
pub struct Messaging<S, C, N> {
    pub(crate) store: S,
    pub(crate) clock: C,
    pub(crate) notifier: N,
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    pub fn new(store: S, clock: C, notifier: N) -> Self {
        Self {
            store,
            clock,
            notifier,
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
    /// The device starts up to date in every current conversation of its user; older
    /// messages are loaded through history.
    pub async fn register_device(&self, device: DeviceId, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        if !tx.user_exists(user).await? {
            return Err(DomainError::UnknownUser);
        }
        tx.insert_device(device, user).await?;
        for (conversation, member) in tx.memberships(user).await? {
            if member.is_current() {
                tx.create_cursor_if_absent(device, conversation.id, conversation.last_seq)
                    .await?;
            }
        }
        tx.commit().await
    }

    pub async fn revoke_device(&self, device: DeviceId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        tx.revoke_device(device).await?;
        tx.commit().await
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

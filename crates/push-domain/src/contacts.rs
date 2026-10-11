use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
use crate::ids::UserId;
use crate::model::ContactState;
use crate::notify::Notifier;
use crate::service::Messaging;
use crate::store::{Store, Tx};
use crate::validation::ValidationError;

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Sends a contact request. Returns the requester's resulting state.
    pub async fn contact_request(&self, from: UserId, to: UserId) -> DomainResult<ContactState> {
        let mut tx = self.begin_pair(from, to).await?;
        let mine = tx.get_contact(from, to).await?;
        let theirs = tx.get_contact(to, from).await?;
        let result = match (mine, theirs) {
            (Some(ContactState::Blocked), _) => return Err(DomainError::Blocked),
            (Some(ContactState::Accepted), _) => ContactState::Accepted,
            (_, Some(ContactState::Blocked)) => {
                // Hide the block: the requester sees an ordinary pending request.
                tx.put_contact(from, to, ContactState::RequestedOut).await?;
                ContactState::RequestedOut
            }
            (_, Some(ContactState::RequestedOut)) => {
                tx.put_contact(from, to, ContactState::Accepted).await?;
                tx.put_contact(to, from, ContactState::Accepted).await?;
                ContactState::Accepted
            }
            _ => {
                tx.put_contact(from, to, ContactState::RequestedOut).await?;
                tx.put_contact(to, from, ContactState::RequestedIn).await?;
                ContactState::RequestedOut
            }
        };
        tx.commit().await?;
        Ok(result)
    }

    /// Accepts a request that `peer` sent to `me`.
    pub async fn contact_accept(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        if tx.get_contact(me, peer).await? != Some(ContactState::RequestedIn) {
            return Err(DomainError::NoPendingRequest);
        }
        tx.put_contact(me, peer, ContactState::Accepted).await?;
        tx.put_contact(peer, me, ContactState::Accepted).await?;
        tx.commit().await
    }

    /// Declines a request that `peer` sent to `me`.
    pub async fn contact_decline(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        if tx.get_contact(me, peer).await? != Some(ContactState::RequestedIn) {
            return Err(DomainError::NoPendingRequest);
        }
        tx.delete_contact(me, peer).await?;
        if tx.get_contact(peer, me).await? == Some(ContactState::RequestedOut) {
            tx.delete_contact(peer, me).await?;
        }
        tx.commit().await
    }

    /// Blocks `peer`. Ends any contact; `peer`'s side is removed unless they also block.
    pub async fn contact_block(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        tx.put_contact(me, peer, ContactState::Blocked).await?;
        if tx.get_contact(peer, me).await? != Some(ContactState::Blocked) {
            tx.delete_contact(peer, me).await?;
        }
        tx.commit().await
    }

    /// Removes a block. A request the block had hidden becomes an ordinary pending request.
    pub async fn contact_unblock(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        if tx.get_contact(me, peer).await? == Some(ContactState::Blocked) {
            tx.delete_contact(me, peer).await?;
            if tx.get_contact(peer, me).await? == Some(ContactState::RequestedOut) {
                tx.put_contact(me, peer, ContactState::RequestedIn).await?;
            }
        }
        tx.commit().await
    }

    /// `me`'s view of the relationship with `peer`.
    pub async fn contact_state(
        &self,
        me: UserId,
        peer: UserId,
    ) -> DomainResult<Option<ContactState>> {
        let mut tx = self.store.begin().await?;
        let state = tx.get_contact(me, peer).await?;
        tx.rollback().await?;
        Ok(state)
    }

    /// Begins a transaction for an operation between two distinct, known users.
    async fn begin_pair(&self, a: UserId, b: UserId) -> DomainResult<S::Tx> {
        if a == b {
            return Err(ValidationError::SelfTarget.into());
        }
        let mut tx = self.store.begin().await?;
        if !tx.user_exists(a).await? || !tx.user_exists(b).await? {
            return Err(DomainError::UnknownUser);
        }
        tx.lock_contact_pair(a, b).await?;
        Ok(tx)
    }
}

/// Whether both directions are `Accepted`.
pub(crate) async fn are_contacts<T: Tx>(tx: &mut T, a: UserId, b: UserId) -> DomainResult<bool> {
    Ok(tx.get_contact(a, b).await? == Some(ContactState::Accepted)
        && tx.get_contact(b, a).await? == Some(ContactState::Accepted))
}

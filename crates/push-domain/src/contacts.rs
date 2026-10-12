use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
use crate::ids::UserId;
use crate::model::ContactState;
use crate::notify::{Event, Notifier};
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
        let (result, their_new) = match (mine, theirs) {
            (Some(ContactState::Blocked), _) => return Err(DomainError::Blocked),
            (Some(ContactState::Accepted), _) => (ContactState::Accepted, theirs),
            // Hide the block: the requester sees an ordinary pending request, and the
            // blocker hears nothing until they unblock.
            (_, Some(ContactState::Blocked)) => (ContactState::RequestedOut, theirs),
            (_, Some(ContactState::RequestedOut)) => {
                (ContactState::Accepted, Some(ContactState::Accepted))
            }
            _ => (ContactState::RequestedOut, Some(ContactState::RequestedIn)),
        };
        let mut events = Vec::new();
        if set_contact(&mut tx, from, to, Some(result)).await? {
            events.push(contact_changed(from, to, Some(result)));
        }
        if set_contact(&mut tx, to, from, their_new).await? {
            events.push(contact_changed(to, from, their_new));
        }
        tx.commit().await?;
        self.publish(events).await;
        Ok(result)
    }

    /// Accepts a request that `peer` sent to `me`.
    pub async fn contact_accept(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        if tx.get_contact(me, peer).await? != Some(ContactState::RequestedIn) {
            return Err(DomainError::NoPendingRequest);
        }
        let accepted = Some(ContactState::Accepted);
        let mut events = Vec::new();
        for (owner, other) in [(me, peer), (peer, me)] {
            if set_contact(&mut tx, owner, other, accepted).await? {
                events.push(contact_changed(owner, other, accepted));
            }
        }
        tx.commit().await?;
        self.publish(events).await;
        Ok(())
    }

    /// Declines a request that `peer` sent to `me`.
    ///
    /// The requester's pending request disappears without an event: a decline must look
    /// the same to them as a block.
    pub async fn contact_decline(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        if tx.get_contact(me, peer).await? != Some(ContactState::RequestedIn) {
            return Err(DomainError::NoPendingRequest);
        }
        set_contact(&mut tx, me, peer, None).await?;
        if tx.get_contact(peer, me).await? == Some(ContactState::RequestedOut) {
            set_contact(&mut tx, peer, me, None).await?;
        }
        tx.commit().await?;
        self.publish(vec![contact_changed(me, peer, None)]).await;
        Ok(())
    }

    /// Blocks `peer`. Ends any contact; `peer`'s side is removed unless they also block.
    /// `peer` is never told.
    pub async fn contact_block(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        let blocked = Some(ContactState::Blocked);
        let mut events = Vec::new();
        if set_contact(&mut tx, me, peer, blocked).await? {
            events.push(contact_changed(me, peer, blocked));
        }
        if tx.get_contact(peer, me).await? != blocked {
            set_contact(&mut tx, peer, me, None).await?;
        }
        tx.commit().await?;
        self.publish(events).await;
        Ok(())
    }

    /// Removes a block. A request the block had hidden becomes an ordinary pending request.
    pub async fn contact_unblock(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let mut tx = self.begin_pair(me, peer).await?;
        let mut events = Vec::new();
        if tx.get_contact(me, peer).await? == Some(ContactState::Blocked) {
            let restored = match tx.get_contact(peer, me).await? {
                Some(ContactState::RequestedOut) => Some(ContactState::RequestedIn),
                _ => None,
            };
            set_contact(&mut tx, me, peer, restored).await?;
            events.push(contact_changed(me, peer, restored));
        }
        tx.commit().await?;
        self.publish(events).await;
        Ok(())
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

/// Writes `owner`'s view of `peer`; `None` removes it. Returns whether it changed.
async fn set_contact<T: Tx>(
    tx: &mut T,
    owner: UserId,
    peer: UserId,
    state: Option<ContactState>,
) -> DomainResult<bool> {
    if tx.get_contact(owner, peer).await? == state {
        return Ok(false);
    }
    match state {
        Some(state) => tx.put_contact(owner, peer, state).await?,
        None => tx.delete_contact(owner, peer).await?,
    }
    Ok(true)
}

fn contact_changed(user: UserId, peer: UserId, state: Option<ContactState>) -> Event {
    Event::ContactChanged { user, peer, state }
}

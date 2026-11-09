use crate::agents::{Resolution, check_agent_limits, owner_events, resolve};
use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
use crate::ids::UserId;
use crate::model::{ContactState, ContactSummary};
use crate::notify::{Event, Notifier};
use crate::outgoing::{Ask, Gate, acting_agent, gate};
use crate::presence::PresenceView;
use crate::service::Messaging;
use crate::store::{Store, Tx};
use crate::validation::ValidationError;

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Trusted auth-svc enrollment, never a public contact API. Projects both verified
    /// accounts and accepts their relationship unless either side has already blocked.
    /// The durable receipt makes replays harmless, including after a later block.
    pub async fn enroll_agent(&self, owner: UserId, agent: UserId) -> DomainResult<()> {
        if owner == agent {
            return Err(ValidationError::SelfTarget.into());
        }
        let mut tx = self.store.begin().await?;
        tx.lock_contact_pair(owner, agent).await?;
        let mut users = [owner, agent];
        users.sort();
        for user in users {
            tx.insert_user(user).await?;
        }
        let mut events = Vec::new();
        if tx.record_agent_owner(agent, owner).await? {
            let blocked = tx.get_contact(owner, agent).await? == Some(ContactState::Blocked)
                || tx.get_contact(agent, owner).await? == Some(ContactState::Blocked);
            if !blocked {
                for (me, peer) in [(owner, agent), (agent, owner)] {
                    if set_contact(&mut tx, me, peer, Some(ContactState::Accepted)).await? {
                        events.push(contact_changed(me, peer, Some(ContactState::Accepted)));
                    }
                }
            }
        }
        tx.commit().await?;
        self.publish(events).await;
        Ok(())
    }

    /// Sends a contact request. Returns the requester's resulting state. A request to an
    /// id that is not a verified user looks like a request nobody answered yet: it stores
    /// nothing and answers `RequestedOut`, so requests never reveal which ids exist.
    ///
    /// A request to an owned agent is resolved at once by the agent's policy (spec
    /// 2026-11-06 agent connections §3): accepted, held for the owner (the agent gets
    /// `RequestedIn` and the owner an [`Event::AgentContact`]), or refused (the requester's
    /// row alone, released later without an event, as a decline looks). A request from an
    /// agent passes the agent's limits first (`RateLimited`).
    ///
    /// An agent's new request (its row toward `to` is absent or `RequestedIn`; `to` is not
    /// its owner) passes its owner's outgoing policy first (spec 2026-11-09 proactive
    /// agents §3): `NotAllowedByOwner`, or `WaitingForOwner` with the waiting action,
    /// before anything that depends on `to`.
    pub async fn contact_request(&self, from: UserId, to: UserId) -> DomainResult<ContactState> {
        if from == to {
            return Err(ValidationError::SelfTarget.into());
        }
        let now = self.clock.now();
        let mut tx = self.store.begin().await?;
        if !tx.user_exists(from).await? {
            return Err(DomainError::UnknownUser);
        }
        let known = tx.user_exists(to).await?;
        if known {
            tx.lock_contact_pair(from, to).await?;
        }
        let owner = tx.agent_owner(from).await?;
        if let Some(owner) = owner.filter(|owner| *owner != to) {
            // The agent's own row only: nothing here depends on `to`.
            let mine = match known {
                true => tx.get_contact(from, to).await?,
                false => None,
            };
            let new = matches!(mine, None | Some(ContactState::RequestedIn));
            if new && acting_agent(&mut tx, from).await?.is_some() {
                if let Gate::Waiting { action, new } =
                    gate(&mut tx, from, Ask::request(to), now).await?
                {
                    return self.wait_for_owner(tx, owner, action, new).await;
                }
            } else {
                check_agent_limits(&mut tx, from, to, now).await?;
            }
        }
        let mut events = Vec::new();
        let result = self
            .request_on(&mut tx, from, to, known, &mut events)
            .await?;
        tx.commit().await?;
        self.publish(events).await;
        Ok(result)
    }

    /// The request of [`Self::contact_request`] past the requester's gate and limits, on
    /// `tx`, which holds the pair lock when `to` is `known` (a verified user). Adds the
    /// events to publish once `tx` commits to `events`.
    pub(crate) async fn request_on(
        &self,
        tx: &mut S::Tx,
        from: UserId,
        to: UserId,
        known: bool,
        events: &mut Vec<Event>,
    ) -> DomainResult<ContactState> {
        if !known {
            return Ok(ContactState::RequestedOut);
        }
        let mine = tx.get_contact(from, to).await?;
        let theirs = tx.get_contact(to, from).await?;
        let mut refuse = false;
        let (result, their_new) = match (mine, theirs) {
            (Some(ContactState::Blocked), _) => return Err(DomainError::Blocked),
            (Some(ContactState::Accepted), _) => (ContactState::Accepted, theirs),
            // Hide the block: the requester sees an ordinary pending request, and the
            // blocker hears nothing until they unblock.
            (_, Some(ContactState::Blocked)) => (ContactState::RequestedOut, theirs),
            (_, Some(ContactState::RequestedOut)) => {
                (ContactState::Accepted, Some(ContactState::Accepted))
            }
            // Already pending: a repeated request changes nothing.
            (_, Some(ContactState::RequestedIn)) => (ContactState::RequestedOut, theirs),
            _ => match tx.agent_owner(to).await? {
                Some(owner) => match resolve(tx, from, to, owner).await? {
                    Resolution::Accept => (ContactState::Accepted, Some(ContactState::Accepted)),
                    Resolution::Hold => {
                        (ContactState::RequestedOut, Some(ContactState::RequestedIn))
                    }
                    Resolution::Refuse => {
                        refuse = true;
                        (ContactState::RequestedOut, theirs)
                    }
                },
                None => (ContactState::RequestedOut, Some(ContactState::RequestedIn)),
            },
        };
        let mut changed = Vec::new();
        if set_contact(tx, from, to, Some(result)).await? {
            changed.push(contact_changed(from, to, Some(result)));
        }
        if set_contact(tx, to, from, their_new).await? {
            changed.push(contact_changed(to, from, their_new));
        }
        if refuse {
            // The requester alone hears of its own request, as for any request; nothing
            // reaches the agent or its owner.
            let release_at = self.refusal_release_at();
            tx.put_refusal(from, to, release_at).await?;
        } else if result == ContactState::Accepted {
            tx.delete_refusal(from, to).await?;
            tx.delete_refusal(to, from).await?;
        }
        owner_events(tx, &mut changed).await?;
        events.extend(changed);
        Ok(result)
    }

    /// Accepts a request that `peer` sent to `me`. An agent's requests are its owner's to
    /// decide: an agent gets `OwnerDecides`.
    pub async fn contact_accept(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        if self.is_owned_agent(me).await? {
            return Err(DomainError::OwnerDecides);
        }
        self.accept_as(me, peer).await
    }

    pub(crate) async fn accept_as(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let Some(mut tx) = self.begin_pair(me, peer).await? else {
            return Err(DomainError::NoPendingRequest);
        };
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
        owner_events(&mut tx, &mut events).await?;
        tx.commit().await?;
        self.publish(events).await;
        Ok(())
    }

    /// Declines a request that `peer` sent to `me`.
    ///
    /// The requester's pending request disappears without an event: a decline must look
    /// the same to them as a block. An agent gets `OwnerDecides`.
    pub async fn contact_decline(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        if self.is_owned_agent(me).await? {
            return Err(DomainError::OwnerDecides);
        }
        self.decline_as(me, peer).await
    }

    pub(crate) async fn decline_as(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let Some(mut tx) = self.begin_pair(me, peer).await? else {
            return Err(DomainError::NoPendingRequest);
        };
        if tx.get_contact(me, peer).await? != Some(ContactState::RequestedIn) {
            return Err(DomainError::NoPendingRequest);
        }
        set_contact(&mut tx, me, peer, None).await?;
        if tx.get_contact(peer, me).await? == Some(ContactState::RequestedOut) {
            set_contact(&mut tx, peer, me, None).await?;
        }
        let mut events = vec![contact_changed(me, peer, None)];
        owner_events(&mut tx, &mut events).await?;
        tx.commit().await?;
        self.publish(events).await;
        Ok(())
    }

    /// Blocks `peer`. Ends any contact; `peer`'s side is removed unless they also block.
    /// `peer` is never told. Blocking an id that is not a verified user stores nothing.
    /// Blocking an accepted contact also publishes `Unknown` presence both ways, after the
    /// commit, so a presence read that still saw the pair as contacts cannot be answered
    /// after it. Both users' locks are held until the commit, so no announcement can follow
    /// it with a stale state (see [`Self::announce_presence`]): one already under way
    /// published before the block took the lock, and a later one leaves the pair out.
    ///
    /// An agent may not block its owner (`NotAllowed`) nor a user whose request waits for
    /// its owner (`OwnerDecides`: that would decline it).
    pub async fn contact_block(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        self.block(me, peer, false).await
    }

    /// The owner's block for its agent `me`: no agent rule applies.
    pub(crate) async fn block_as(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        self.block(me, peer, true).await
    }

    async fn block(&self, me: UserId, peer: UserId, by_owner: bool) -> DomainResult<()> {
        let Some(mut tx) = self.begin_pair(me, peer).await? else {
            if !by_owner {
                let mut tx = self.store.begin().await?;
                let owner = tx.agent_owner(me).await?;
                tx.rollback().await?;
                if owner == Some(peer) {
                    return Err(DomainError::NotAllowed);
                }
            }
            return Ok(());
        };
        if !by_owner {
            // Checked under the pair lock, so no request can arrive in between.
            if let Some(owner) = tx.agent_owner(me).await? {
                if owner == peer {
                    return Err(DomainError::NotAllowed);
                }
                if tx.get_contact(me, peer).await? == Some(ContactState::RequestedIn) {
                    return Err(DomainError::OwnerDecides);
                }
            }
        }
        tx.lock_users(&[me, peer]).await?;
        let blocked = Some(ContactState::Blocked);
        let mut events = Vec::new();
        // The block records whether `me` could see `peer` until now, so that blocking a
        // stranger never makes them visible. A block already in place keeps its record.
        let previous = tx.get_contact(me, peer).await?;
        let theirs = tx.get_contact(peer, me).await?;
        // Accepted contacts saw each other's presence; the block ends that both ways. Each
        // side gets the same `Unknown` an opt-out gives, so the blocked user learns nothing.
        let presence_ended =
            previous == Some(ContactState::Accepted) && theirs == Some(ContactState::Accepted);
        if previous != blocked {
            let sees_peer = matches!(
                previous,
                Some(ContactState::RequestedIn | ContactState::Accepted)
            );
            tx.put_block(me, peer, sees_peer).await?;
            events.push(contact_changed(me, peer, blocked));
        }
        // The blocked side (and, for an agent, its owner) hears nothing: a block looks
        // like a decline.
        if theirs != blocked {
            set_contact(&mut tx, peer, me, None).await?;
        }
        owner_events(&mut tx, &mut events).await?;
        tx.commit().await?;
        let mut published: Vec<Event> = Vec::new();
        if presence_ended {
            published.extend([(peer, me), (me, peer)].into_iter().map(|(to, user)| {
                Event::Presence {
                    to: vec![to],
                    user,
                    view: PresenceView::Unknown,
                }
            }));
        }
        published.extend(events);
        self.publish(published).await;
        Ok(())
    }

    /// Removes a block. A request the block had hidden becomes an ordinary pending request.
    pub async fn contact_unblock(&self, me: UserId, peer: UserId) -> DomainResult<()> {
        let Some(mut tx) = self.begin_pair(me, peer).await? else {
            return Ok(());
        };
        let mut events = Vec::new();
        if tx.get_contact(me, peer).await? == Some(ContactState::Blocked) {
            let restored = match tx.get_contact(peer, me).await? {
                Some(ContactState::RequestedOut) => Some(ContactState::RequestedIn),
                _ => None,
            };
            set_contact(&mut tx, me, peer, restored).await?;
            events.push(contact_changed(me, peer, restored));
        }
        owner_events(&mut tx, &mut events).await?;
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

    /// Every relationship `user` has, as they see it, sorted by peer. Includes the users
    /// they have blocked; never shows that someone blocked them.
    pub async fn list_contacts(&self, user: UserId) -> DomainResult<Vec<ContactSummary>> {
        let mut tx = self.store.begin().await?;
        let rows = tx.contacts_of(user).await?;
        tx.rollback().await?;
        let mut contacts: Vec<ContactSummary> = rows
            .into_iter()
            .map(|(user_id, state)| ContactSummary { user_id, state })
            .collect();
        contacts.sort_by_key(|contact| contact.user_id);
        Ok(contacts)
    }

    /// The users among `candidates` whose profiles `user` may see (spec §5.5): `user`
    /// themselves, everyone they share or shared a conversation with, and the peers of
    /// their contact rows as they see them: requests they received and accepted contacts,
    /// and users they blocked, but only if they could see that user when they blocked them
    /// (their row was a received request, an accepted contact or a block that counted).
    /// Blocking a stranger grants nothing, or the stranger's own block would reveal
    /// whoever blocked them. Their own pending request to someone grants nothing either.
    /// Sorted, without duplicates. Someone who blocked `user` stays hidden unless they
    /// share a conversation, so nobody learns of a block through it.
    pub async fn visible_users(
        &self,
        user: UserId,
        candidates: &[UserId],
    ) -> DomainResult<Vec<UserId>> {
        let mut tx = self.store.begin().await?;
        let mut visible = tx.related_users(user, candidates).await?;
        tx.rollback().await?;
        if candidates.contains(&user) {
            visible.push(user);
        }
        visible.sort();
        visible.dedup();
        Ok(visible)
    }

    /// Begins a transaction for an operation of `me` on `peer`, two distinct users. `None`
    /// when `peer` is not a verified user: the operation then answers as it would for a
    /// user `me` has no relationship with, so no answer reveals which ids exist (spec
    /// §5.5). An unknown `me` is `UnknownUser`: the gateway only acts for known devices.
    async fn begin_pair(&self, me: UserId, peer: UserId) -> DomainResult<Option<S::Tx>> {
        if me == peer {
            return Err(ValidationError::SelfTarget.into());
        }
        let mut tx = self.store.begin().await?;
        if !tx.user_exists(me).await? {
            return Err(DomainError::UnknownUser);
        }
        if !tx.user_exists(peer).await? {
            tx.rollback().await?;
            return Ok(None);
        }
        tx.lock_contact_pair(me, peer).await?;
        Ok(Some(tx))
    }
}

/// Whether both directions are `Accepted`.
pub(crate) async fn are_contacts<T: Tx>(tx: &mut T, a: UserId, b: UserId) -> DomainResult<bool> {
    Ok(tx.get_contact(a, b).await? == Some(ContactState::Accepted)
        && tx.get_contact(b, a).await? == Some(ContactState::Accepted))
}

/// Writes `owner`'s view of `peer`; `None` removes it. Returns whether it changed.
pub(crate) async fn set_contact<T: Tx>(
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

pub(crate) fn contact_changed(user: UserId, peer: UserId, state: Option<ContactState>) -> Event {
    Event::ContactChanged { user, peer, state }
}

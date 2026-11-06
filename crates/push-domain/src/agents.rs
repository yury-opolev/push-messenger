//! Agent connections (spec 2026-11-06 agent connections and the owner view): requests to
//! agents resolved by their owner's policy, the agents' own request limits, the owner's
//! actions on an agent's relationships, and the release of refused requests.

use chrono::{DateTime, TimeDelta, Utc};
use uuid::Uuid;

use crate::clock::Clock;
use crate::contacts::{are_contacts, contact_changed, set_contact};
use crate::error::{DomainError, DomainResult};
use crate::ids::UserId;
use crate::model::{AgentPolicy, ContactState, ContactSummary};
use crate::notify::{Event, Notifier};
use crate::service::Messaging;
use crate::store::{Store, Tx};
use crate::validation::ValidationError;

/// Contact requests an agent may send per hour.
pub const AGENT_REQUESTS_PER_HOUR: u32 = 20;
/// Contact requests an agent may send per 24 hours.
pub const AGENT_REQUESTS_PER_DAY: u32 = 100;
/// Pending outgoing requests an agent may have.
pub const AGENT_PENDING_OUT_MAX: u32 = 50;
/// Outgoing requests of an agent that may wait for other agents' owners at once.
pub const AGENT_WAITING_OUT_MAX: u32 = 10;
/// Requests that may wait for an agent's owner; later ones are refused.
pub const AGENT_WAITING_IN_MAX: u32 = 200;
/// `retry_after_s` of the caps on pending and waiting requests.
const CAP_RETRY_S: u32 = 600;
/// Refused requests released per sweep.
const RELEASE_BATCH: u32 = 100;

/// How long a refused request stays pending before it disappears: uniformly random
/// between `min` and `max`, so its end looks like a decline at an unknown time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefusalDelay {
    min: TimeDelta,
    max: TimeDelta,
}

impl Default for RefusalDelay {
    /// 10 minutes to 6 hours.
    fn default() -> Self {
        Self::new(TimeDelta::minutes(10), TimeDelta::hours(6))
    }
}

impl RefusalDelay {
    /// `min` below zero counts as zero, `max` below `min` as `min`.
    pub fn new(min: TimeDelta, max: TimeDelta) -> Self {
        let min = min.max(TimeDelta::zero());
        Self {
            min,
            max: max.max(min),
        }
    }

    fn pick(self) -> TimeDelta {
        let span = (self.max - self.min).num_milliseconds();
        if span <= 0 {
            return self.min;
        }
        let random = Uuid::new_v4().as_u64_pair().0 % (span as u64 + 1);
        self.min + TimeDelta::milliseconds(random as i64)
    }
}

/// What a request to an agent becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resolution {
    /// Both sides `Accepted`.
    Accept,
    /// The agent gets `RequestedIn`: the request waits for the owner.
    Hold,
    /// Only the requester's `RequestedOut` row, released later without an event.
    Refuse,
}

/// The owner's answer to a waiting request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerAction {
    Approve,
    Decline,
    Block,
}

/// What `agent`'s policy makes of a new request from `from`; `owner` owns `agent`.
pub(crate) async fn resolve<T: Tx>(
    tx: &mut T,
    from: UserId,
    agent: UserId,
    owner: UserId,
) -> DomainResult<Resolution> {
    if from == owner {
        return Ok(Resolution::Accept);
    }
    if tx.is_user_deleted(agent).await? {
        return Ok(Resolution::Refuse);
    }
    let policy = tx.agent_policy(agent).await?.unwrap_or_default();
    let accepted = match policy {
        AgentPolicy::AutoAny => true,
        AgentPolicy::RefuseAll => return Ok(Resolution::Refuse),
        AgentPolicy::Ask => false,
        AgentPolicy::AutoOwn => tx.agent_owner(from).await? == Some(owner),
        AgentPolicy::AutoOwnerContacts => match tx.agent_owner(from).await? {
            Some(theirs) if theirs == owner => true,
            Some(theirs) => are_contacts(tx, owner, theirs).await?,
            None => are_contacts(tx, owner, from).await?,
        },
    };
    if accepted {
        return Ok(Resolution::Accept);
    }
    if tx.waiting_in_count(agent).await? >= AGENT_WAITING_IN_MAX {
        return Ok(Resolution::Refuse);
    }
    Ok(Resolution::Hold)
}

/// An agent's limits on its own requests (spec §4), checked under its user lock before
/// anything is stored; the request is logged when it passes.
pub(crate) async fn check_agent_limits<T: Tx>(
    tx: &mut T,
    agent: UserId,
    peer: UserId,
    now: DateTime<Utc>,
) -> DomainResult<()> {
    tx.lock_users(&[agent]).await?;
    for (window, max) in [
        (TimeDelta::hours(1), AGENT_REQUESTS_PER_HOUR),
        (TimeDelta::days(1), AGENT_REQUESTS_PER_DAY),
    ] {
        let (count, oldest) = tx.agent_requests_since(agent, now - window).await?;
        if count >= max {
            let free_at = oldest.unwrap_or(now) + window;
            let seconds = (free_at - now).num_seconds().max(1);
            return Err(DomainError::RateLimited {
                retry_after_s: u32::try_from(seconds).unwrap_or(u32::MAX),
            });
        }
    }
    let (pending, waiting) = tx.outgoing_counts(agent).await?;
    if pending >= AGENT_PENDING_OUT_MAX || waiting >= AGENT_WAITING_OUT_MAX {
        return Err(DomainError::RateLimited {
            retry_after_s: CAP_RETRY_S,
        });
    }
    tx.log_agent_request(agent, peer, now).await
}

/// Adds an [`Event::AgentContact`] for the owner of every owned agent among the users of
/// `events`' contact changes (the owner's own relationship excepted: the owner hears
/// that as their own contact event).
pub(crate) async fn owner_events<T: Tx>(tx: &mut T, events: &mut Vec<Event>) -> DomainResult<()> {
    let mut added = Vec::new();
    for event in events.iter() {
        let Event::ContactChanged { user, peer, state } = event else {
            continue;
        };
        if let Some(owner) = tx.agent_owner(*user).await? {
            if owner != *peer {
                added.push(Event::AgentContact {
                    owner,
                    agent: *user,
                    user: *peer,
                    state: *state,
                });
            }
        }
    }
    events.extend(added);
    Ok(())
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Whether `user` is an owned agent.
    pub(crate) async fn is_owned_agent(&self, user: UserId) -> DomainResult<bool> {
        let mut tx = self.store.begin().await?;
        let agent = tx.agent_owner(user).await?.is_some();
        tx.rollback().await?;
        Ok(agent)
    }

    /// `AgentNotFound` unless `agent` is an owned agent of `owner` that was not deleted.
    pub(crate) async fn require_owned(&self, owner: UserId, agent: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        let owned =
            tx.agent_owner(agent).await? == Some(owner) && !tx.is_user_deleted(agent).await?;
        tx.rollback().await?;
        if owned {
            Ok(())
        } else {
            Err(DomainError::AgentNotFound)
        }
    }

    /// The agent's policy, for its owner.
    pub async fn agent_policy(&self, owner: UserId, agent: UserId) -> DomainResult<AgentPolicy> {
        self.require_owned(owner, agent).await?;
        let mut tx = self.store.begin().await?;
        let policy = tx.agent_policy(agent).await?.unwrap_or_default();
        tx.rollback().await?;
        Ok(policy)
    }

    /// Sets the agent's policy, for its owner. Requests already waiting keep waiting.
    pub async fn set_agent_policy(
        &self,
        owner: UserId,
        agent: UserId,
        policy: AgentPolicy,
    ) -> DomainResult<AgentPolicy> {
        self.require_owned(owner, agent).await?;
        let mut tx = self.store.begin().await?;
        tx.set_agent_policy(agent, policy, self.clock.now()).await?;
        tx.commit().await?;
        Ok(policy)
    }

    /// The requests waiting for the owner, oldest first, with when each arrived.
    pub async fn waiting_requests(
        &self,
        owner: UserId,
        agent: UserId,
    ) -> DomainResult<Vec<(UserId, DateTime<Utc>)>> {
        self.require_owned(owner, agent).await?;
        let mut tx = self.store.begin().await?;
        let waiting = tx.waiting_requests(agent).await?;
        tx.rollback().await?;
        Ok(waiting)
    }

    /// The owner approves, declines or blocks `user` for the agent, as the agent would.
    /// Approving or declining without a waiting request is `NoPendingRequest`; the owner
    /// as `user` is `NotAllowed`.
    pub async fn resolve_request(
        &self,
        owner: UserId,
        agent: UserId,
        user: UserId,
        action: OwnerAction,
    ) -> DomainResult<()> {
        self.require_owned(owner, agent).await?;
        if user == owner {
            return Err(DomainError::NotAllowed);
        }
        match action {
            OwnerAction::Approve => self.accept_as(agent, user).await,
            OwnerAction::Decline => self.decline_as(agent, user).await,
            OwnerAction::Block => self.block_as(agent, user).await,
        }
    }

    /// The agent's relationships as the agent sees them, sorted by user, for its owner.
    pub async fn agent_contacts(
        &self,
        owner: UserId,
        agent: UserId,
    ) -> DomainResult<Vec<ContactSummary>> {
        self.require_owned(owner, agent).await?;
        self.list_contacts(agent).await
    }

    /// Ends the agent's relationship with `user`, whatever it is, for its owner: an
    /// accepted contact or the agent's own request (both rows go, both users hear `None`;
    /// a block of `user`'s stays), a waiting request (declined) or the agent's block
    /// (removed). Idempotent. The owner is `NotAllowed`.
    pub async fn remove_agent_contact(
        &self,
        owner: UserId,
        agent: UserId,
        user: UserId,
    ) -> DomainResult<()> {
        self.require_owned(owner, agent).await?;
        if user == owner {
            return Err(DomainError::NotAllowed);
        }
        if agent == user {
            return Err(ValidationError::SelfTarget.into());
        }
        let mut tx = self.store.begin().await?;
        if !tx.user_exists(user).await? {
            tx.rollback().await?;
            return Ok(());
        }
        tx.lock_contact_pair(agent, user).await?;
        let mine = tx.get_contact(agent, user).await?;
        tx.rollback().await?;
        match mine {
            Some(ContactState::RequestedIn) => return self.decline_as(agent, user).await,
            Some(ContactState::Blocked) => return self.contact_unblock(agent, user).await,
            Some(ContactState::Accepted | ContactState::RequestedOut) => {}
            None => return Ok(()),
        }
        let mut tx = self.store.begin().await?;
        tx.lock_contact_pair(agent, user).await?;
        let mut events = Vec::new();
        if matches!(
            tx.get_contact(agent, user).await?,
            Some(ContactState::Accepted | ContactState::RequestedOut)
        ) {
            set_contact(&mut tx, agent, user, None).await?;
            events.push(contact_changed(agent, user, None));
            if matches!(
                tx.get_contact(user, agent).await?,
                Some(ContactState::Accepted | ContactState::RequestedIn)
            ) {
                set_contact(&mut tx, user, agent, None).await?;
                events.push(contact_changed(user, agent, None));
            }
        }
        owner_events(&mut tx, &mut events).await?;
        tx.commit().await?;
        self.publish(events).await;
        Ok(())
    }

    /// Releases the refused requests that are due: each requester's pending request
    /// disappears without an event, as a decline looks to them. Also forgets agents'
    /// request log entries older than a day. Returns how many requests were released.
    pub async fn release_refusals(&self) -> DomainResult<u32> {
        let now = self.clock.now();
        let mut tx = self.store.begin().await?;
        let due = tx.due_refusals(now, RELEASE_BATCH).await?;
        tx.prune_agent_requests(now - TimeDelta::days(1)).await?;
        tx.commit().await?;
        let mut released = 0;
        for (requester, agent) in due {
            let mut tx = self.store.begin().await?;
            tx.lock_contact_pair(requester, agent).await?;
            if tx.delete_refusal(requester, agent).await?
                && tx.get_contact(requester, agent).await? == Some(ContactState::RequestedOut)
                && tx.get_contact(agent, requester).await?.is_none()
            {
                tx.delete_contact(requester, agent).await?;
                released += 1;
            }
            tx.commit().await?;
        }
        Ok(released)
    }

    /// When the next refusal is released, from now: a random delay.
    pub(crate) fn refusal_release_at(&self) -> DateTime<Utc> {
        self.clock.now() + self.refusal_delay.pick()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_delays_stay_within_their_range() {
        let delay = RefusalDelay::new(TimeDelta::seconds(10), TimeDelta::seconds(20));
        for _ in 0..200 {
            let picked = delay.pick();
            assert!(picked >= TimeDelta::seconds(10) && picked <= TimeDelta::seconds(20));
        }
        let fixed = RefusalDelay::new(TimeDelta::seconds(5), TimeDelta::seconds(1));
        assert_eq!(fixed.pick(), TimeDelta::seconds(5));
        let zero = RefusalDelay::new(TimeDelta::seconds(-5), TimeDelta::zero());
        assert_eq!(zero.pick(), TimeDelta::zero());
    }
}

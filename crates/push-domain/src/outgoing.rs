//! Proactive agents (spec 2026-11-09 proactive agents): the owner's outgoing policy on
//! what an agent starts on its own, the queue of actions that wait for the owner, their
//! approval, decline and expiry, the agent's limits per kind, and the agent's settings.

use chrono::{DateTime, TimeDelta, Utc};
use uuid::Uuid;

use crate::agents::{check_agent_limits, check_pending_caps};
use crate::clock::Clock;
use crate::conversations::{create_direct, existing_direct};
use crate::error::{DomainError, DomainResult};
use crate::groups::{approved_invite, create_group_on, group_parts};
use crate::ids::{ConversationId, UserId};
use crate::model::{
    AgentSettings, InboundPolicy, OutgoingAction, OutgoingError, OutgoingKind, OutgoingPolicy,
    OutgoingPolicyChange, OutgoingState,
};
use crate::notify::{Event, Notifier};
use crate::service::Messaging;
use crate::store::{Store, Tx};

/// New direct chats (not with the owner) an agent may open per 24 hours.
pub const CHATS_PER_DAY: u32 = 50;
/// Calls to people other than the owner an agent may place per 24 hours.
pub const CALLS_PER_DAY: u32 = 10;
/// Groups an agent may create per 24 hours.
pub const GROUPS_PER_DAY: u32 = 5;
/// Members an agent may add to groups per 24 hours.
pub const INVITES_PER_DAY: u32 = 20;
/// Actions of an agent that may wait for its owner at once.
pub const WAITING_ACTIONS_MAX: u32 = 20;
/// `retry_after_s` of the cap on waiting actions.
const WAITING_RETRY_S: u32 = 600;
/// How long an action waits for the owner before it expires.
pub const OUTGOING_EXPIRY: TimeDelta = TimeDelta::hours(24);
/// How long a resolved action is kept (the agent lists it to catch up).
pub const RESOLVED_KEPT: TimeDelta = TimeDelta::hours(24);
/// How long an approved call lets the agent ring that person, once.
pub const CALL_GRANT: TimeDelta = TimeDelta::minutes(15);
/// The window of the per-kind limits, and how long the action log is kept.
const LIMIT_WINDOW: TimeDelta = TimeDelta::days(1);
/// Waiting actions expired per sweep.
const EXPIRY_BATCH: u32 = 100;

/// What an agent asks to do: the parts of an [`OutgoingAction`] that identify it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ask {
    kind: OutgoingKind,
    target: Option<UserId>,
    conversation: Option<ConversationId>,
    title: Option<String>,
    /// Sorted, without duplicates.
    members: Vec<UserId>,
}

impl Ask {
    pub(crate) fn request(target: UserId) -> Self {
        Self::to(OutgoingKind::Request, target)
    }

    pub(crate) fn chat(target: UserId) -> Self {
        Self::to(OutgoingKind::Chat, target)
    }

    // Used by the calls gate (spec 2026-11-09 proactive agents §3.1, `CallInvite`).
    #[allow(dead_code)]
    pub(crate) fn call(target: UserId) -> Self {
        Self::to(OutgoingKind::Call, target)
    }

    /// A group titled `title` (validated) with `members` (sorted, without duplicates).
    pub(crate) fn group(title: String, members: Vec<UserId>) -> Self {
        Self {
            kind: OutgoingKind::Group,
            target: None,
            conversation: None,
            title: Some(title),
            members,
        }
    }

    /// `members` (sorted, without duplicates) added to `group`.
    pub(crate) fn invite(group: ConversationId, members: Vec<UserId>) -> Self {
        Self {
            kind: OutgoingKind::Invite,
            target: None,
            conversation: Some(group),
            title: None,
            members,
        }
    }

    fn to(kind: OutgoingKind, target: UserId) -> Self {
        Self {
            kind,
            target: Some(target),
            conversation: None,
            title: None,
            members: Vec::new(),
        }
    }

    /// How many the action counts for its limit: each member added, else one.
    fn count(&self) -> u32 {
        match self.kind {
            OutgoingKind::Invite => u32::try_from(self.members.len()).unwrap_or(u32::MAX),
            _ => 1,
        }
    }

    fn into_action(self, agent: UserId, now: DateTime<Utc>) -> OutgoingAction {
        OutgoingAction {
            action_id: Uuid::new_v4(),
            agent,
            kind: self.kind,
            target: self.target,
            conversation: self.conversation,
            title: self.title,
            members: self.members,
            state: OutgoingState::Waiting,
            error: None,
            created_at: now,
            resolved_at: None,
            used_at: None,
        }
    }
}

/// What the gate makes of an action.
#[derive(Debug)]
pub(crate) enum Gate {
    /// Carry it out (counted).
    Proceed,
    /// It waits for the owner. `new`: stored now (the owner is told), else asked again.
    Waiting { action: OutgoingAction, new: bool },
}

/// The owner of `user` when it is an owned agent that was not deleted: the actions it
/// starts on its own pass the gate.
pub(crate) async fn acting_agent<T: Tx>(tx: &mut T, user: UserId) -> DomainResult<Option<UserId>> {
    match tx.agent_owner(user).await? {
        Some(owner) if !tx.is_user_deleted(user).await? => Ok(Some(owner)),
        _ => Ok(None),
    }
}

/// The owner's outgoing policy on `agent`'s action (spec §3.2), before anything that
/// depends on the other side, so its answer reveals nothing about the target: `off`
/// refuses, `allowed` counts it against the agent's limits, `ask` stores it as a waiting
/// action (or finds the same one waiting) and counts it. Takes the agent's user lock, so
/// concurrent actions cannot pass a limit together: a path that locks other users too
/// locks them all, the agent included, before it calls the gate.
pub(crate) async fn gate<T: Tx>(
    tx: &mut T,
    agent: UserId,
    ask: Ask,
    now: DateTime<Utc>,
) -> DomainResult<Gate> {
    tx.lock_users(&[agent]).await?;
    if tx.is_user_deleted(agent).await? {
        // Deleted after the caller's check: answered as a deleted agent is, which passes
        // no gate (its contact requests keep their limits).
        if ask.kind == OutgoingKind::Request {
            count(tx, agent, &ask, now).await?;
        }
        return Ok(Gate::Proceed);
    }
    match tx.agent_settings(agent).await?.policy(ask.kind) {
        OutgoingPolicy::Off => Err(DomainError::NotAllowedByOwner),
        OutgoingPolicy::Allowed => {
            count(tx, agent, &ask, now).await?;
            Ok(Gate::Proceed)
        }
        OutgoingPolicy::Ask => {
            let action = ask.clone().into_action(agent, now);
            if let Some(action) = tx.find_waiting_outgoing(&action).await? {
                return Ok(Gate::Waiting { action, new: false });
            }
            if tx.waiting_outgoing_count(agent).await? >= WAITING_ACTIONS_MAX {
                return Err(DomainError::RateLimited {
                    retry_after_s: WAITING_RETRY_S,
                });
            }
            count(tx, agent, &ask, now).await?;
            tx.insert_outgoing(&action).await?;
            Ok(Gate::Waiting { action, new: true })
        }
    }
}

/// Checks `agent`'s limit for the action and logs it. Contact requests keep their own
/// limiter (with the caps on pending requests).
async fn count<T: Tx>(
    tx: &mut T,
    agent: UserId,
    ask: &Ask,
    now: DateTime<Utc>,
) -> DomainResult<()> {
    let max = match ask.kind {
        OutgoingKind::Request => {
            let target = ask.target.unwrap_or(agent);
            return check_agent_limits(tx, agent, target, now).await;
        }
        OutgoingKind::Chat => CHATS_PER_DAY,
        OutgoingKind::Group => GROUPS_PER_DAY,
        OutgoingKind::Invite => INVITES_PER_DAY,
        OutgoingKind::Call => CALLS_PER_DAY,
    };
    let count = ask.count();
    let (used, oldest) = tx
        .agent_actions_since(agent, ask.kind, now - LIMIT_WINDOW)
        .await?;
    if used.saturating_add(count) > max {
        let free_at = oldest.unwrap_or(now) + LIMIT_WINDOW;
        let seconds = (free_at - now).num_seconds().max(1);
        return Err(DomainError::RateLimited {
            retry_after_s: u32::try_from(seconds).unwrap_or(u32::MAX),
        });
    }
    tx.log_agent_action(agent, ask.kind, count, now).await
}

/// Uses an approved call of `agent` to `callee` approved within [`CALL_GRANT`] before
/// `now`, if there is one: the call passes the gate once. Returns whether one was used.
// Used by the calls gate (spec 2026-11-09 proactive agents §3.3).
#[allow(dead_code)]
pub(crate) async fn use_call_grant<T: Tx>(
    tx: &mut T,
    agent: UserId,
    callee: UserId,
    now: DateTime<Utc>,
) -> DomainResult<bool> {
    match tx
        .approved_call_grant(agent, callee, now - CALL_GRANT)
        .await?
    {
        Some(action_id) => tx.use_outgoing(action_id, now).await,
        None => Ok(false),
    }
}

/// `AgentNotFound` when `agent` is deleted; `OutgoingNotFound` unless `action_id` is a
/// waiting action of `agent`. The caller holds the agent's user lock.
pub(crate) async fn still_waiting<T: Tx>(
    tx: &mut T,
    agent: UserId,
    action_id: Uuid,
) -> DomainResult<()> {
    if tx.is_user_deleted(agent).await? {
        return Err(DomainError::AgentNotFound);
    }
    match tx.outgoing(action_id).await? {
        Some(action) if action.agent == agent && action.state == OutgoingState::Waiting => Ok(()),
        _ => Err(DomainError::OutgoingNotFound),
    }
}

/// The failure an approved action ends with when the normal rules refuse it; `None` for
/// an error that is no such answer (storage, conflicts, the action or the agent gone),
/// which leaves the action waiting.
fn failure(error: &DomainError) -> Option<OutgoingError> {
    let code = match error {
        DomainError::NotContact => OutgoingError::NotContact,
        DomainError::Blocked => OutgoingError::Blocked,
        DomainError::NotAdmin => OutgoingError::NotAdmin,
        DomainError::NotMember => OutgoingError::NotMember,
        DomainError::GroupFull => OutgoingError::GroupFull,
        DomainError::NotAGroup => OutgoingError::NotAGroup,
        DomainError::ConversationNotFound => OutgoingError::ConversationNotFound,
        DomainError::NotAllowed | DomainError::OwnerDecides => OutgoingError::NotAllowed,
        DomainError::RateLimited { .. } => OutgoingError::RateLimited,
        DomainError::Invalid(_) => OutgoingError::Invalid,
        DomainError::Conflict
        | DomainError::Store(_)
        | DomainError::KickFailed(_)
        | DomainError::DeviceNotKnownYet
        | DomainError::AgentNotFound
        | DomainError::OutgoingNotFound
        | DomainError::WaitingForOwner { .. }
        | DomainError::NotAllowedByOwner => return None,
        _ => OutgoingError::Failed,
    };
    Some(code)
}

/// What carrying out an approved action did.
enum Carried {
    /// Done, with the conversation it opened or created, if any.
    Done(Option<ConversationId>),
    /// Nothing to do (every member to add is one already); what it wrote is undone.
    Unchanged,
}

/// The events telling the owner and the agent of a resolved action.
fn resolved_events(owner: UserId, action: &OutgoingAction) -> Vec<Event> {
    vec![
        Event::Outgoing {
            to: owner,
            action: action.clone(),
        },
        Event::Outgoing {
            to: action.agent,
            action: action.clone(),
        },
    ]
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Ends an operation the gate made wait: commits the transaction (with the new
    /// action), tells the owner of a new one, and answers `WaitingForOwner`.
    pub(crate) async fn wait_for_owner<T>(
        &self,
        tx: S::Tx,
        owner: UserId,
        action: OutgoingAction,
        new: bool,
    ) -> DomainResult<T> {
        tx.commit().await?;
        let action_id = action.action_id;
        if new {
            self.publish(vec![Event::Outgoing { to: owner, action }])
                .await;
        }
        Err(DomainError::WaitingForOwner { action_id })
    }

    /// The gate on its own transaction, for a path that cannot commit its own when the
    /// action waits ([`Self::add_members`]): `Ok` when the action may be carried out, and
    /// counted; otherwise the gate's answer.
    pub(crate) async fn gate_alone(
        &self,
        agent: UserId,
        owner: UserId,
        ask: Ask,
    ) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        match gate(&mut tx, agent, ask, self.clock.now()).await? {
            Gate::Proceed => tx.commit().await,
            Gate::Waiting { action, new } => self.wait_for_owner(tx, owner, action, new).await,
        }
    }

    /// The agent's settings, for its owner.
    pub async fn agent_settings(
        &self,
        owner: UserId,
        agent: UserId,
    ) -> DomainResult<AgentSettings> {
        self.require_owned(owner, agent).await?;
        let mut tx = self.store.begin().await?;
        let settings = tx.agent_settings(agent).await?;
        tx.rollback().await?;
        Ok(settings)
    }

    /// Changes some of the agent's outgoing policies, for its owner, and tells the agent.
    /// Actions already waiting keep waiting. Returns the settings now stored.
    pub async fn set_outgoing_policy(
        &self,
        owner: UserId,
        agent: UserId,
        change: OutgoingPolicyChange,
    ) -> DomainResult<AgentSettings> {
        self.change_settings(owner, agent, |settings| change.apply(settings))
            .await
    }

    /// Sets who reaches the agent's backend, for its owner, and tells the agent. Returns
    /// the settings now stored.
    pub async fn set_inbound_policy(
        &self,
        owner: UserId,
        agent: UserId,
        inbound: InboundPolicy,
    ) -> DomainResult<AgentSettings> {
        self.change_settings(owner, agent, |settings| AgentSettings {
            inbound,
            ..settings
        })
        .await
    }

    async fn change_settings(
        &self,
        owner: UserId,
        agent: UserId,
        change: impl FnOnce(AgentSettings) -> AgentSettings,
    ) -> DomainResult<AgentSettings> {
        self.require_owned(owner, agent).await?;
        let mut tx = self.store.begin().await?;
        // Serialises changes of one agent's settings, so none is lost.
        tx.lock_users(&[agent]).await?;
        if tx.is_user_deleted(agent).await? {
            tx.rollback().await?;
            return Err(DomainError::AgentNotFound);
        }
        let settings = change(tx.agent_settings(agent).await?);
        tx.set_agent_settings(agent, &settings, self.clock.now())
            .await?;
        tx.commit().await?;
        self.publish(vec![Event::AgentSettings { agent, settings }])
            .await;
        Ok(settings)
    }

    /// The agent's actions waiting for its owner, oldest first, for its owner.
    pub async fn waiting_outgoing(
        &self,
        owner: UserId,
        agent: UserId,
    ) -> DomainResult<Vec<OutgoingAction>> {
        self.require_owned(owner, agent).await?;
        let mut tx = self.store.begin().await?;
        let actions = tx.list_outgoing(agent, None).await?;
        tx.rollback().await?;
        Ok(actions)
    }

    /// The owner approves a waiting action: it is carried out at once, as the agent, with
    /// the gate and the rate limits skipped (it was counted when it was queued); a contact
    /// request still keeps the caps on pending requests. A call is not placed: the agent
    /// may ring that person once within [`CALL_GRANT`]. When the normal rules refuse it,
    /// the action ends `Failed` with their error; that is still an answer, not an error.
    /// The owner and the agent are told. Returns the action as now stored. An action that
    /// is not waiting, or not this agent's, is `OutgoingNotFound`.
    ///
    /// One transaction: the action is carried out and resolved together, under the
    /// agent's lock (taken after the locks carrying it out needs first, see
    /// [`crate::store`]). An error that is no answer of the rules (a conflict, storage)
    /// changes nothing: the action keeps waiting.
    pub async fn approve_outgoing(
        &self,
        owner: UserId,
        agent: UserId,
        action_id: Uuid,
    ) -> DomainResult<OutgoingAction> {
        self.require_owned(owner, agent).await?;
        let now = self.clock.now();
        let mut tx = self.store.begin().await?;
        // Read without a lock, to know which locks carrying it out takes; checked again
        // under the agent's lock.
        let action = match tx.outgoing(action_id).await? {
            Some(action) if action.agent == agent && action.state == OutgoingState::Waiting => {
                action
            }
            _ => {
                tx.rollback().await?;
                return Err(DomainError::OutgoingNotFound);
            }
        };
        tx.savepoint().await?;
        let mut events = Vec::new();
        let carried = self.carry_out(&mut tx, &action, now, &mut events).await;
        let undo = !matches!(carried, Ok(Carried::Done(_)));
        let (to, error, conversation) = match carried {
            Ok(Carried::Done(conversation)) => (OutgoingState::Approved, None, conversation),
            Ok(Carried::Unchanged) => (OutgoingState::Approved, None, None),
            Err(error) => match failure(&error) {
                Some(code) => (OutgoingState::Failed, Some(code), None),
                // Dropping `tx` rolls it back.
                None => return Err(error),
            },
        };
        if undo {
            // Undo what was written, which also gives back the locks taken since; the
            // agent's lock is taken again, and the action checked again under it.
            tx.rollback_to_savepoint().await?;
            events.clear();
            tx.lock_users(&[agent]).await?;
            still_waiting(&mut tx, agent, action_id).await?;
        }
        let resolved = tx
            .resolve_outgoing(
                action_id,
                OutgoingState::Waiting,
                to,
                error,
                conversation,
                now,
            )
            .await?
            .ok_or(DomainError::OutgoingNotFound)?;
        tx.commit().await?;
        events.extend(resolved_events(owner, &resolved));
        self.publish(events).await;
        Ok(resolved)
    }

    /// Carries out `action` (waiting, read without a lock) on `tx` as its agent: takes the
    /// locks it needs in their order, the agent's among them, then checks
    /// [`still_waiting`] under the agent's lock, then does it. Adds the events to publish
    /// once `tx` commits to `events`.
    async fn carry_out(
        &self,
        tx: &mut S::Tx,
        action: &OutgoingAction,
        now: DateTime<Utc>,
        events: &mut Vec<Event>,
    ) -> DomainResult<Carried> {
        let agent = action.agent;
        let missing = |what: &str| DomainError::Store(format!("{:?} without {what}", action.kind));
        match action.kind {
            OutgoingKind::Request => {
                let target = action.target.ok_or_else(|| missing("a target"))?;
                let known = tx.user_exists(target).await?;
                if known {
                    tx.lock_contact_pair(agent, target).await?;
                }
                tx.lock_users(&[agent]).await?;
                still_waiting(tx, agent, action.action_id).await?;
                check_pending_caps(tx, agent).await?;
                self.request_on(tx, agent, target, known, events).await?;
                Ok(Carried::Done(None))
            }
            OutgoingKind::Chat => {
                let peer = action.target.ok_or_else(|| missing("a target"))?;
                tx.lock_contact_pair(agent, peer).await?;
                tx.lock_users(&[agent, peer]).await?;
                still_waiting(tx, agent, action.action_id).await?;
                if let Some(existing) = existing_direct(tx, agent, peer).await? {
                    return Ok(Carried::Done(Some(existing)));
                }
                let by_agent = tx.agent_owner(agent).await? != Some(peer);
                match create_direct(tx, agent, peer, by_agent, now).await? {
                    Some(id) => Ok(Carried::Done(Some(id))),
                    // Created concurrently, which the pair lock rules out: retry.
                    None => Err(DomainError::Conflict),
                }
            }
            OutgoingKind::Group => {
                let title = action.title.as_deref().ok_or_else(|| missing("a title"))?;
                let (title, others) = group_parts(agent, title, &action.members)?;
                let mut users = others.clone();
                users.push(agent);
                tx.lock_users(&users).await?;
                still_waiting(tx, agent, action.action_id).await?;
                let (id, created) = create_group_on(tx, agent, title, &others, true, now).await?;
                events.extend(created);
                Ok(Carried::Done(Some(id)))
            }
            OutgoingKind::Invite => {
                let group = action.conversation.ok_or_else(|| missing("a group"))?;
                let added =
                    approved_invite(tx, agent, action.action_id, group, &action.members, now)
                        .await?;
                match added {
                    Some(added) => {
                        events.extend(added);
                        Ok(Carried::Done(None))
                    }
                    None => Ok(Carried::Unchanged),
                }
            }
            OutgoingKind::Call => {
                tx.lock_users(&[agent]).await?;
                still_waiting(tx, agent, action.action_id).await?;
                Ok(Carried::Done(None))
            }
        }
    }

    /// The owner declines a waiting action. The owner and the agent are told. Returns the
    /// action as now stored. An action that is not waiting, or not this agent's, is
    /// `OutgoingNotFound`.
    pub async fn decline_outgoing(
        &self,
        owner: UserId,
        agent: UserId,
        action_id: Uuid,
    ) -> DomainResult<OutgoingAction> {
        self.require_owned(owner, agent).await?;
        let mut tx = self.store.begin().await?;
        tx.lock_users(&[agent]).await?;
        still_waiting(&mut tx, agent, action_id).await?;
        let declined = tx
            .resolve_outgoing(
                action_id,
                OutgoingState::Waiting,
                OutgoingState::Declined,
                None,
                None,
                self.clock.now(),
            )
            .await?
            .ok_or(DomainError::OutgoingNotFound)?;
        tx.commit().await?;
        self.publish(resolved_events(owner, &declined)).await;
        Ok(declined)
    }

    /// An agent's own settings. People are `NotAllowed`.
    pub async fn own_agent_settings(&self, agent: UserId) -> DomainResult<AgentSettings> {
        let mut tx = self.store.begin().await?;
        if tx.agent_owner(agent).await?.is_none() {
            tx.rollback().await?;
            return Err(DomainError::NotAllowed);
        }
        let settings = tx.agent_settings(agent).await?;
        tx.rollback().await?;
        Ok(settings)
    }

    /// An agent's own actions: those waiting and those resolved within [`RESOLVED_KEPT`],
    /// oldest first. People are `NotAllowed`.
    pub async fn own_outgoing_actions(&self, agent: UserId) -> DomainResult<Vec<OutgoingAction>> {
        let mut tx = self.store.begin().await?;
        if tx.agent_owner(agent).await?.is_none() {
            tx.rollback().await?;
            return Err(DomainError::NotAllowed);
        }
        let actions = tx
            .list_outgoing(agent, Some(self.clock.now() - RESOLVED_KEPT))
            .await?;
        tx.rollback().await?;
        Ok(actions)
    }

    /// The sweeper's part for outgoing actions: actions waiting for [`OUTGOING_EXPIRY`]
    /// end `Expired` (the owner and the agent are told), resolved actions go
    /// [`RESOLVED_KEPT`] after their resolution, and action log rows after a day. Returns
    /// how many actions expired.
    pub(crate) async fn sweep_outgoing(&self) -> DomainResult<u32> {
        let now = self.clock.now();
        let mut tx = self.store.begin().await?;
        let expired = tx
            .expire_outgoing(now - OUTGOING_EXPIRY, now, EXPIRY_BATCH)
            .await?;
        let mut events = Vec::with_capacity(expired.len() * 2);
        for action in &expired {
            if let Some(owner) = tx.agent_owner(action.agent).await? {
                events.push(Event::Outgoing {
                    to: owner,
                    action: action.clone(),
                });
            }
            events.push(Event::Outgoing {
                to: action.agent,
                action: action.clone(),
            });
        }
        tx.prune_outgoing(now - RESOLVED_KEPT).await?;
        tx.prune_agent_actions(now - LIMIT_WINDOW).await?;
        tx.commit().await?;
        self.publish(events).await;
        Ok(u32::try_from(expired.len()).unwrap_or(u32::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{service, user};

    /// An agent deleted after the caller's check and before the gate's lock is answered
    /// as a deleted agent is: its action is not queued (nothing would tell the owner).
    #[tokio::test]
    async fn the_gate_queues_nothing_for_an_agent_deleted_before_its_lock() {
        let (svc, _) = service();
        let (owner, agent, peer) = (user(&svc).await, user(&svc).await, user(&svc).await);
        svc.enroll_agent(owner, agent).await.unwrap();
        let now = Utc::now();
        let mut tx = svc.store.begin().await.unwrap();
        tx.mark_user_deleted(agent, now).await.unwrap();
        let answer = gate(&mut tx, agent, Ask::chat(peer), now).await.unwrap();
        assert!(matches!(answer, Gate::Proceed), "{answer:?}");
        assert_eq!(tx.waiting_outgoing_count(agent).await, Ok(0));
        assert_eq!(
            tx.agent_actions_since(agent, OutgoingKind::Chat, now).await,
            Ok((0, None))
        );
    }
}

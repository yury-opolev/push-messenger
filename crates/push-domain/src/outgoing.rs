//! Proactive agents (spec 2026-11-09 proactive agents): the owner's outgoing policy on
//! what an agent starts on its own, the queue of actions that wait for the owner, their
//! approval, decline and expiry, the agent's limits per kind, and the agent's settings.

use chrono::{DateTime, TimeDelta, Utc};
use uuid::Uuid;

use crate::agents::check_agent_limits;
use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
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

/// The failure an approved action ends with when the normal rules refuse it.
fn failure(error: &DomainError) -> OutgoingError {
    match error {
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
        _ => OutgoingError::Failed,
    }
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
    /// the gate skipped (it was counted when it was queued). A call is not placed: the
    /// agent may ring that person once within [`CALL_GRANT`]. When the normal rules
    /// refuse it, the action ends `Failed` with their error; that is still an answer, not
    /// an error. The owner and the agent are told. Returns the action as now stored. An
    /// action that is not waiting, or not this agent's, is `OutgoingNotFound`.
    pub async fn approve_outgoing(
        &self,
        owner: UserId,
        agent: UserId,
        action_id: Uuid,
    ) -> DomainResult<OutgoingAction> {
        let approved = self
            .resolve_waiting(owner, agent, action_id, OutgoingState::Approved)
            .await?;
        let outcome = match approved.kind {
            OutgoingKind::Request => match approved.target {
                Some(target) => self
                    .contact_request_as(agent, target, false)
                    .await
                    .map(|_| None),
                None => Err(DomainError::Store("a request without a target".into())),
            },
            OutgoingKind::Chat => match approved.target {
                Some(target) => self.open_direct_as(agent, target, false).await.map(Some),
                None => Err(DomainError::Store("a chat without a target".into())),
            },
            OutgoingKind::Group => {
                let title = approved.title.clone().unwrap_or_default();
                self.create_group_as(agent, &title, &approved.members, false)
                    .await
                    .map(Some)
            }
            OutgoingKind::Invite => match approved.conversation {
                Some(group) => self
                    .add_members_as(agent, group, &approved.members, false)
                    .await
                    .map(|()| None),
                None => Err(DomainError::Store("an invite without a group".into())),
            },
            OutgoingKind::Call => Ok(None),
        };
        let now = self.clock.now();
        let (to, error, conversation) = match outcome {
            Ok(None) => {
                self.tell_resolved(owner, approved.clone()).await;
                return Ok(approved);
            }
            Ok(Some(conversation)) => (OutgoingState::Approved, None, Some(conversation)),
            Err(error) => (OutgoingState::Failed, Some(failure(&error)), None),
        };
        let mut tx = self.store.begin().await?;
        let action = tx
            .resolve_outgoing(
                action_id,
                OutgoingState::Approved,
                to,
                error,
                conversation,
                now,
            )
            .await?;
        tx.commit().await?;
        // Gone only if the agent was deleted meanwhile.
        let action = action.ok_or(DomainError::OutgoingNotFound)?;
        self.tell_resolved(owner, action.clone()).await;
        Ok(action)
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
        let declined = self
            .resolve_waiting(owner, agent, action_id, OutgoingState::Declined)
            .await?;
        self.tell_resolved(owner, declined.clone()).await;
        Ok(declined)
    }

    /// Moves `agent`'s waiting action to `to`, under the agent's lock.
    async fn resolve_waiting(
        &self,
        owner: UserId,
        agent: UserId,
        action_id: Uuid,
        to: OutgoingState,
    ) -> DomainResult<OutgoingAction> {
        self.require_owned(owner, agent).await?;
        let mut tx = self.store.begin().await?;
        tx.lock_users(&[agent]).await?;
        let mine = tx
            .outgoing(action_id)
            .await?
            .is_some_and(|action| action.agent == agent);
        let resolved = match mine {
            true => {
                tx.resolve_outgoing(
                    action_id,
                    OutgoingState::Waiting,
                    to,
                    None,
                    None,
                    self.clock.now(),
                )
                .await?
            }
            false => None,
        };
        let Some(resolved) = resolved else {
            tx.rollback().await?;
            return Err(DomainError::OutgoingNotFound);
        };
        tx.commit().await?;
        Ok(resolved)
    }

    /// Tells the owner and the agent of a resolved action.
    async fn tell_resolved(&self, owner: UserId, action: OutgoingAction) {
        let agent = action.agent;
        self.publish(vec![
            Event::Outgoing {
                to: owner,
                action: action.clone(),
            },
            Event::Outgoing { to: agent, action },
        ])
        .await;
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

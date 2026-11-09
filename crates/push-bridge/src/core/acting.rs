//! Bridge actions in the core (spec 2026-11-09 proactive agents §7.1, §7.3): a backend's
//! [`BackendEvent::Action`](crate::backend::BackendEvent::Action) runs here, inside or
//! outside a turn. Whatever needs the server runs in a task and comes back as
//! [`Internal::Acted`]; the core then sends (the loop guard's place, see
//! [`Core::guard_delivery`]), places a call or keeps what an approval still needs.
//!
//! Actions waiting for the owner are kept in `state.json` (`pending_actions`) by their id
//! and finished when the server reports the owner's decision
//! (`Event::OutgoingActionChanged`), or, for those decided while the bridge was away, at
//! start-up and after each reconnect ([`Core::reconcile_actions`]).
//!
//! Logs carry the action's kind and result code only (`agent_action`): never a text, a
//! handle or a group's name.

use push_sdk::{ContactState, ConversationId, OutgoingAction, OutgoingState, SdkError, UserId};
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::calling::Purpose;
use super::{Core, Internal};
use crate::actions::{
    Action, ActionCode, ActionRequest, ActionResult, Target, group_name, handle, parse_group,
};
use crate::messenger::{Messenger, Peer};
use crate::render::formatted_bodies;
use crate::state::{PendingAction, PendingKind};

/// Kept waiting actions older than this are dropped (spec §7.3).
const PENDING_KEPT_MS: i64 = 48 * 3_600 * 1_000;

/// What a backend's action came to once the server answered.
pub(super) enum Acted {
    /// Nothing left to do but answer.
    Finished(ActionResult),
    /// The owner decides: `pending` is kept under the result's action id.
    Waiting {
        result: ActionResult,
        pending: PendingKind,
    },
    /// Send `text` into `conversation`: plain to an agent, a rich reply to a person or a
    /// group; `to` names it in the result.
    Deliver {
        conversation: ConversationId,
        to: String,
        agent: bool,
        text: String,
    },
    /// A message to the owner (a handle that turned out to be the owner's).
    ToOwner { text: String },
    /// A call to the owner.
    CallOwner { reason: Option<String> },
    /// A call to someone else.
    CallContact { peer: Peer, reason: Option<String> },
}

impl<M: Messenger> Core<M> {
    /// Runs a backend's action; its result goes back on the request's oneshot.
    pub(super) async fn start_action(&mut self, request: ActionRequest) {
        let ActionRequest { action, reply } = request;
        let kind = action.kind();
        let acted = match action {
            Action::NotifyOwner { text } => Acted::Finished(self.notify_owner(&text)),
            Action::SendMessage {
                to: Target::Owner,
                text,
            } => Acted::ToOwner { text },
            Action::Call {
                to: Target::Owner,
                reason,
            } => Acted::CallOwner { reason },
            Action::Call {
                to: Target::Group(_),
                ..
            } => Acted::Finished(ActionResult::new(
                ActionCode::NotAllowed,
                "a group cannot be called",
            )),
            action => {
                let messenger = self.messenger.clone();
                let internal = self.internal.clone();
                let owner = self.owner;
                tokio::spawn(async move {
                    let acted = run(messenger.as_ref(), owner, action).await;
                    let _ = internal.send(Internal::Acted { kind, reply, acted });
                });
                return;
            }
        };
        self.acted(kind, reply, acted).await;
    }

    /// Finishes an action the server answered.
    pub(super) async fn acted(
        &mut self,
        kind: &'static str,
        reply: oneshot::Sender<ActionResult>,
        acted: Acted,
    ) {
        let result = match acted {
            Acted::Finished(result) => result,
            Acted::Waiting { result, pending } => {
                if let Some(id) = &result.action_id {
                    self.keep_pending(id, pending);
                }
                result
            }
            Acted::Deliver {
                conversation,
                to,
                agent,
                text,
            } => self.deliver_text(conversation, &to, agent, &text),
            Acted::ToOwner { text } => self.send_to_owner(&text),
            Acted::CallOwner { reason } => self.call_owner_for_agent(reason).await,
            Acted::CallContact { peer, reason } => self.call_contact(&peer, reason).await,
        };
        tracing::info!(event = "agent_action", kind, code = result.code.as_str());
        let _ = reply.send(result);
    }

    /// `notify_owner`: a note titled with the backend's name in the owner's chat, as its
    /// background messages are.
    fn notify_owner(&self, text: &str) -> ActionResult {
        let bodies = formatted_bodies(Some(&self.name), text, self.format);
        if bodies.iter().all(|body| self.send_rich(body).is_some()) {
            ActionResult::done("the owner was notified")
        } else {
            ActionResult::failed("the note could not be sent")
        }
    }

    /// `send_message` to the owner: a reply in the owner's chat.
    fn send_to_owner(&self, text: &str) -> ActionResult {
        let bodies = formatted_bodies(None, text, self.format);
        if bodies.iter().all(|body| self.send_rich(body).is_some()) {
            ActionResult::done("sent to the owner").with_conversation(self.conversation.to_string())
        } else {
            ActionResult::failed("the message could not be sent")
        }
    }

    /// Sends the backend's `text` into `conversation`: plain text to an agent (a turn
    /// for its backend, counted by the loop guard), a rich reply to a person or a group.
    fn deliver_text(
        &mut self,
        conversation: ConversationId,
        to: &str,
        agent: bool,
        text: &str,
    ) -> ActionResult {
        if let Some(refused) = self.guard_delivery(conversation) {
            return refused;
        }
        let sent = if agent {
            self.messenger
                .send_text(conversation, text)
                .map(|_| self.agent_turn(conversation, to.trim_start_matches('@')))
                .map_err(|error| error.code().to_owned())
        } else {
            formatted_bodies(None, text, self.format)
                .iter()
                .try_for_each(|body| {
                    self.messenger
                        .send_rich(conversation, body)
                        .map(drop)
                        .map_err(|error| error.code().to_owned())
                })
        };
        match sent {
            Ok(()) => {
                let conversation = if to.starts_with(crate::actions::GROUP_PREFIX) {
                    to.to_owned()
                } else {
                    conversation.to_string()
                };
                ActionResult::done(format!("sent to {to}")).with_conversation(conversation)
            }
            Err(code) => ActionResult::failed(format!("the message could not be sent ({code})")),
        }
    }

    /// Whether the backend may send into `conversation` now; `Some` refuses it: the loop
    /// guard answers `paused` for a chat it paused.
    fn guard_delivery(&self, conversation: ConversationId) -> Option<ActionResult> {
        self.loop_refusal(conversation)
    }

    /// `call` to the owner: dialed as for a request (one call at a time, voice ready, the
    /// minimum interval between calls), whatever the notify-by-call switch and the quiet
    /// hours say.
    async fn call_owner_for_agent(&mut self, reason: Option<String>) -> ActionResult {
        if let Some(why) = self.dial_blocker() {
            return ActionResult::new(ActionCode::Unavailable, why);
        }
        let interval = self
            .voice
            .as_ref()
            .map(|voice| voice.policy.notify.min_interval)
            .unwrap_or_default();
        if let Some(last) = self.last_dialed {
            let since = Instant::now().saturating_duration_since(last);
            if since < interval {
                let mut result = ActionResult::new(
                    ActionCode::Unavailable,
                    "the owner was called a short while ago",
                );
                let wait = (interval - since).as_secs().max(1);
                result.retry_after_s = Some(u32::try_from(wait).unwrap_or(u32::MAX));
                return result;
            }
        }
        self.dial(Purpose::Agent { reason }).await;
        ActionResult::done("calling the owner")
    }

    /// `call` to someone other than the owner (spec §7.5). Agents never call agents;
    /// calls to people are not carried out by this bridge.
    async fn call_contact(&mut self, peer: &Peer, _reason: Option<String>) -> ActionResult {
        if peer.agent {
            return ActionResult::new(ActionCode::NotAllowed, "agents cannot call other agents");
        }
        ActionResult::failed("not implemented: this bridge calls only its owner")
    }

    /// Keeps what the approval of action `id` still needs.
    fn keep_pending(&self, id: &str, kind: PendingKind) {
        let pending = PendingAction {
            kind,
            created_at_ms: self.clock.now_ms(),
        };
        self.save(|state| {
            state.pending_actions.insert(id.to_owned(), pending);
        });
    }

    /// The server says one of the agent's actions was decided: an approved one is
    /// finished (the held text sent, the person dialed), and the owner is told the
    /// outcome of every action this bridge started.
    pub(super) async fn outgoing_changed(&mut self, action: OutgoingAction) {
        if action.agent != self.messenger.me() || action.state == OutgoingState::Waiting {
            return;
        }
        let Some(pending) = self.take_pending(&action.action_id) else {
            tracing::debug!(
                event = "agent_action_unknown",
                state = action.state.as_str()
            );
            return;
        };
        tracing::info!(
            event = "agent_action_decided",
            kind = action.kind.as_str(),
            state = action.state.as_str()
        );
        let what = pending.kind.describe();
        match action.state {
            OutgoingState::Approved => self.approved(pending.kind, action.conversation).await,
            OutgoingState::OwnerDeclined | OutgoingState::Expired => {
                self.reply(&format!("Owner declined: {what}."));
            }
            OutgoingState::Failed => {
                let code = action.error.as_deref().unwrap_or("failed");
                self.reply(&format!("Failed: {what} ({code})."));
            }
            OutgoingState::Waiting => {}
        }
    }

    fn take_pending(&self, id: &str) -> Option<PendingAction> {
        let mut taken = None;
        self.save(|state| taken = state.pending_actions.remove(id));
        taken
    }

    /// The owner approved: what was held is done now.
    async fn approved(&mut self, kind: PendingKind, conversation: Option<ConversationId>) {
        match kind {
            PendingKind::Send { .. } => match conversation {
                Some(conversation) => self.approved_send(kind, Ok(conversation)),
                None => {
                    let PendingKind::Send { target, .. } = &kind else {
                        return;
                    };
                    let Ok(target) = target.parse::<UserId>() else {
                        return self.approved_send(kind, Err(SdkError::Invalid("target".into())));
                    };
                    let messenger = self.messenger.clone();
                    let internal = self.internal.clone();
                    tokio::spawn(async move {
                        let result = messenger.open_direct(target).await;
                        let _ = internal.send(Internal::ApprovedChat { kind, result });
                    });
                }
            },
            PendingKind::Call {
                target,
                handle,
                reason,
            } => {
                let result = match target.parse::<UserId>() {
                    Ok(user_id) => {
                        let peer = Peer {
                            user_id,
                            handle: handle.clone(),
                            agent: false,
                            owner: None,
                        };
                        self.call_contact(&peer, reason).await
                    }
                    Err(_) => ActionResult::failed("the kept call names no user"),
                };
                tracing::info!(
                    event = "agent_action",
                    kind = "call",
                    code = result.code.as_str()
                );
                if result.code == ActionCode::Done {
                    self.reply(&format!("Owner approved: calling @{handle}."));
                } else {
                    self.reply(&format!(
                        "Owner approved the call to @{handle}, but it was not placed: {}.",
                        result.summary()
                    ));
                }
            }
            PendingKind::Request { handle } => {
                self.reply(&format!(
                    "Owner approved: contact request to @{handle} sent."
                ));
            }
            PendingKind::Group { name } => {
                let id = conversation.map(group_name);
                self.reply(&match id {
                    Some(id) => format!("Owner approved: group \"{name}\" created ({id})."),
                    None => format!("Owner approved: group \"{name}\" created."),
                });
            }
            PendingKind::Invite { group, handle } => {
                self.reply(&format!("Owner approved: @{handle} added to {group}."));
            }
        }
    }

    /// An approved chat is open (or could not be opened): the held text goes into it.
    pub(super) fn approved_send(
        &mut self,
        kind: PendingKind,
        conversation: Result<ConversationId, SdkError>,
    ) {
        let PendingKind::Send {
            handle,
            text,
            agent,
            ..
        } = kind
        else {
            return;
        };
        let result = match conversation {
            Ok(conversation) => {
                self.deliver_text(conversation, &format!("@{handle}"), agent, &text)
            }
            Err(error) => ActionResult::from_sdk(&error),
        };
        tracing::info!(
            event = "agent_action",
            kind = "send_message",
            code = result.code.as_str()
        );
        if result.code == ActionCode::Done {
            self.reply(&format!("Owner approved: message to @{handle} sent."));
        } else {
            self.reply(&format!(
                "Owner approved the chat with @{handle}, but the message was not sent: {}.",
                result.summary()
            ));
        }
    }

    /// At start-up and after a reconnect: kept actions older than 48 hours are dropped,
    /// and the agent's actions are listed to settle those decided meanwhile.
    pub(super) fn reconcile_actions(&self) {
        let now = self.clock.now_ms();
        let mut dropped = 0usize;
        self.save(|state| {
            state.pending_actions.retain(|_, pending| {
                let keep = now.saturating_sub(pending.created_at_ms) < PENDING_KEPT_MS;
                dropped += usize::from(!keep);
                keep
            });
        });
        if dropped > 0 {
            tracing::info!(event = "agent_actions_dropped", count = dropped);
        }
        if self.state.get().pending_actions.is_empty() {
            return;
        }
        let messenger = self.messenger.clone();
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let result = messenger.outgoing_actions().await;
            let _ = internal.send(Internal::Reconciled { result });
        });
    }

    /// The agent's actions as the server lists them: the decided ones are settled.
    pub(super) async fn reconciled(&mut self, result: Result<Vec<OutgoingAction>, SdkError>) {
        let actions = match result {
            Ok(actions) => actions,
            Err(error) => {
                tracing::warn!(event = "agent_actions_list_failed", code = error.code());
                return;
            }
        };
        for action in actions {
            self.outgoing_changed(action).await;
        }
    }
}

/// Runs the server's part of `action` for the agent whose owner is `owner`.
async fn run<M: Messenger + ?Sized>(messenger: &M, owner: UserId, action: Action) -> Acted {
    run_checked(messenger, owner, action)
        .await
        .unwrap_or_else(Acted::Finished)
}

/// [`run`], with `Err` for an early answer (an unknown handle or group).
async fn run_checked<M: Messenger + ?Sized>(
    messenger: &M,
    owner: UserId,
    action: Action,
) -> Result<Acted, ActionResult> {
    Ok(match action {
        Action::RequestContact { handle } => {
            let peer = find(messenger, &handle).await?;
            match messenger.request_contact(peer.user_id).await {
                Ok(state) => Acted::Finished(ActionResult::done(contact_detail(state, &peer))),
                Err(error) => refused(
                    &error,
                    PendingKind::Request {
                        handle: peer.handle,
                    },
                ),
            }
        }
        Action::SendMessage {
            to: Target::Handle(handle),
            text,
        } => {
            let peer = find(messenger, &handle).await?;
            if peer.user_id == owner {
                return Ok(Acted::ToOwner { text });
            }
            match messenger.open_direct(peer.user_id).await {
                Ok(conversation) => Acted::Deliver {
                    conversation,
                    to: format!("@{}", peer.handle),
                    agent: peer.agent,
                    text,
                },
                Err(error) => refused(
                    &error,
                    PendingKind::Send {
                        target: peer.user_id.to_string(),
                        handle: peer.handle,
                        text,
                        agent: peer.agent,
                    },
                ),
            }
        }
        Action::SendMessage {
            to: Target::Group(group),
            text,
        } => match messenger.is_group_member(group).await {
            Ok(true) => Acted::Deliver {
                conversation: group,
                to: group_name(group),
                agent: false,
                text,
            },
            Ok(false) => Acted::Finished(ActionResult::new(
                ActionCode::NotFound,
                format!("not a member of {}", group_name(group)),
            )),
            Err(error) => Acted::Finished(ActionResult::from_sdk(&error)),
        },
        Action::SendMessage {
            to: Target::Owner,
            text,
        } => Acted::ToOwner { text },
        Action::CreateGroup { name, members } => {
            let mut ids = Vec::with_capacity(members.len());
            for member in &members {
                ids.push(find(messenger, member).await?.user_id);
            }
            match messenger.create_group(&name, ids).await {
                Ok(group) => Acted::Finished(
                    ActionResult::done(format!("group \"{name}\" created ({})", group_name(group)))
                        .with_conversation(group_name(group)),
                ),
                Err(error) => groups_refused(&error, PendingKind::Group { name }),
            }
        }
        Action::Invite { group, member } => {
            let Some(group) = parse_group(&group) else {
                return Err(ActionResult::new(
                    ActionCode::NotFound,
                    format!("not a group: {group}"),
                ));
            };
            let peer = find(messenger, &member).await?;
            match messenger.add_members(group, vec![peer.user_id]).await {
                Ok(()) => Acted::Finished(
                    ActionResult::done(format!("@{} added to {}", peer.handle, group_name(group)))
                        .with_conversation(group_name(group)),
                ),
                Err(error) => groups_refused(
                    &error,
                    PendingKind::Invite {
                        group: group_name(group),
                        handle: peer.handle,
                    },
                ),
            }
        }
        Action::Call {
            to: Target::Handle(handle),
            reason,
        } => {
            let peer = find(messenger, &handle).await?;
            if peer.user_id == owner {
                Acted::CallOwner { reason }
            } else {
                Acted::CallContact { peer, reason }
            }
        }
        Action::Call {
            to: Target::Owner,
            reason,
        } => Acted::CallOwner { reason },
        Action::Call {
            to: Target::Group(_),
            ..
        } => Acted::Finished(ActionResult::new(
            ActionCode::NotAllowed,
            "a group cannot be called",
        )),
        Action::Outgoing => Acted::Finished(outgoing(messenger).await),
        Action::NotifyOwner { .. } => {
            Acted::Finished(ActionResult::failed("notify_owner runs in the core"))
        }
    })
}

/// The user with `handle`; `not_found` when there is none.
async fn find<M: Messenger + ?Sized>(messenger: &M, text: &str) -> Result<Peer, ActionResult> {
    let Some(handle) = handle(text) else {
        return Err(ActionResult::new(
            ActionCode::NotFound,
            format!("not a handle: {text}"),
        ));
    };
    messenger.lookup(&handle).await.map_err(|error| {
        let mut result = ActionResult::from_sdk(&error);
        if result.code == ActionCode::NotFound {
            result.detail = format!("no user @{handle}");
        }
        result
    })
}

/// The server's refusal; `waiting_for_owner` keeps `pending` for the approval.
fn refused(error: &SdkError, pending: PendingKind) -> Acted {
    let result = ActionResult::from_sdk(error);
    if result.code == ActionCode::WaitingForOwner {
        Acted::Waiting { result, pending }
    } else {
        Acted::Finished(result)
    }
}

/// [`refused`] for groups: a server without groups says so.
fn groups_refused(error: &SdkError, pending: PendingKind) -> Acted {
    if error.code() == "not_available" {
        return Acted::Finished(ActionResult::new(
            ActionCode::Unavailable,
            "groups are not enabled on this server",
        ));
    }
    refused(error, pending)
}

/// The relationship after a contact request, for the backend.
fn contact_detail(state: ContactState, peer: &Peer) -> String {
    let handle = &peer.handle;
    match state {
        ContactState::Accepted => format!("@{handle} is a contact"),
        ContactState::Blocked => format!("@{handle} is blocked"),
        ContactState::RequestedIn => format!("@{handle} asked first; the owner decides"),
        ContactState::RequestedOut | ContactState::None => {
            format!("contact request sent to @{handle}")
        }
    }
}

/// `outgoing`: how many actions wait, and one line per action.
async fn outgoing<M: Messenger + ?Sized>(messenger: &M) -> ActionResult {
    let actions = match messenger.outgoing_actions().await {
        Ok(actions) => actions,
        Err(error) => return ActionResult::from_sdk(&error),
    };
    let targets: Vec<UserId> = actions.iter().filter_map(|action| action.target).collect();
    let peers = if targets.is_empty() {
        Vec::new()
    } else {
        messenger.peers(targets).await.unwrap_or_default()
    };
    let waiting = actions
        .iter()
        .filter(|action| action.state == OutgoingState::Waiting)
        .count();
    let mut detail = format!("{waiting} waiting");
    for action in &actions {
        let target = match (action.target, &action.title, action.conversation) {
            (Some(user), ..) => peers
                .iter()
                .find(|peer| peer.user_id == user)
                .map_or_else(|| user.to_string(), |peer| format!("@{}", peer.handle)),
            (None, Some(title), _) => format!("\"{title}\""),
            (None, None, Some(group)) => group_name(group),
            (None, None, None) => "-".to_owned(),
        };
        let mut state = action.state.as_str().to_owned();
        if let Some(error) = &action.error {
            state.push_str(&format!(" ({error})"));
        }
        detail.push_str(&format!("\n{} {target}: {state}", action.kind.as_str()));
    }
    ActionResult::done(detail)
}

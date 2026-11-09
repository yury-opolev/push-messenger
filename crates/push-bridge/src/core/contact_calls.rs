//! The agent's calls to its contacts (spec 2026-11-09 proactive agents §7.5): the
//! backend's `call` action to a person who is an accepted contact, or the owner's approval
//! of such a call, goes out through the voice adapter as the owner's calls do (one call at
//! a time, hang-up, mute, the tray's call state).
//!
//! The server gates the invite with the owner's `calls` policy and answers only a refusal:
//! the action's result is sent when the phone fails, when the server refuses the invite
//! (`CallEvent::Error`), or, without a refusal, [`CONFIRM`] after the invite (the call
//! rings), at pickup or at the call's end, whichever comes first. The core never waits
//! for it.
//!
//! During the call the person's speech goes to the backend as that contact's message
//! (`contact_prompt`) and is posted in their direct chat; the backend's replies to them are
//! spoken and posted. A call never opens a chat (that is a gated action of its own): with
//! no direct chat yet, both are posted in the owner's chat, labelled with their handle.
//! An approval lets one call through within 15 minutes; one settled later is not dialed.
//! The inbound policy does not apply: the agent itself called them.
//! The owner's session is never prompted from this call and its turns are not spoken.
//!
//! Logs carry the action's kind and result code only: never a handle or a reason.

use push_sdk::{CallEndReason, SdkError, UserId};
use tokio::sync::oneshot;
use tokio::time::{Duration, Instant};

use super::Core;
use super::calling::Purpose;
use super::contacts::ContactChat;
use crate::actions::{ActionCode, ActionResult};
use crate::messenger::{Messenger, Peer};
use crate::state::PendingKind;
use crate::voice::speech;

/// Without a refusal from the server this long after the invite, the call rings.
const CONFIRM: Duration = Duration::from_secs(3);
/// The title of a contact's words on a call, posted in their chat.
pub(super) const TRANSCRIPT_TITLE: &str = "🎤 voice";

/// Who learns how a call to a contact went.
pub(super) enum Caller {
    /// The backend's `call` action.
    Action {
        kind: &'static str,
        reply: oneshot::Sender<ActionResult>,
    },
    /// The owner approved the call: a note in the owner's chat.
    Approved,
}

/// A call to a contact whose result was not told yet.
pub(super) struct ContactCall {
    caller: Caller,
    peer: UserId,
    handle: String,
    reason: Option<String>,
    /// Without a refusal by then the call rings; `None` until the phone placed it.
    confirm_at: Option<Instant>,
}

impl<M: Messenger> Core<M> {
    /// Calls `peer` for `caller`: agents are never called; the backend must take
    /// contacts' messages, and the bridge must be able to call now (as for the owner).
    pub(super) async fn call_contact(
        &mut self,
        peer: Peer,
        reason: Option<String>,
        caller: Caller,
    ) {
        let refused = if peer.agent {
            Some(ActionResult::new(
                ActionCode::NotAllowed,
                "agents cannot call other agents",
            ))
        } else if let Some(why) = self.dial_blocker() {
            Some(ActionResult::new(ActionCode::Unavailable, why))
        } else if !self.accepts_contacts {
            Some(ActionResult::new(
                ActionCode::Unavailable,
                "the backend does not talk with contacts",
            ))
        } else {
            None
        };
        let call = ContactCall {
            caller,
            peer: peer.user_id,
            handle: peer.handle,
            reason,
            confirm_at: None,
        };
        if let Some(result) = refused {
            return self.tell_caller(call, result);
        }
        // Never opened for a call: without a chat of their own, the call is posted in
        // the owner's chat.
        let chat = match self.known_direct(call.peer).await {
            Some(conversation) => ContactChat::Direct(conversation),
            None => ContactChat::Owner {
                handle: call.handle.clone(),
            },
        };
        let purpose = Purpose::Contact {
            peer: call.peer,
            handle: call.handle.clone(),
            chat,
            reason: call.reason.clone(),
        };
        self.contact_call = Some(call);
        self.dial(purpose).await;
    }

    /// Tells the caller how the call to a contact went: a `waiting_for_owner` call is
    /// kept for the owner's approval.
    fn tell_caller(&mut self, call: ContactCall, result: ActionResult) {
        let ContactCall {
            caller,
            peer,
            handle,
            reason,
            ..
        } = call;
        let (kind, reply) = match caller {
            Caller::Action { kind, reply } => (kind, Some(reply)),
            Caller::Approved => ("call", None),
        };
        tracing::info!(event = "agent_action", kind, code = result.code.as_str());
        // Waiting for the owner (again, even after an approval): kept, so the owner's
        // approval dials.
        if result.code == ActionCode::WaitingForOwner {
            if let Some(id) = &result.action_id {
                self.keep_pending(
                    id,
                    PendingKind::Call {
                        target: peer.to_string(),
                        handle: handle.clone(),
                        reason,
                    },
                );
            }
        }
        let Some(reply) = reply else {
            return self.reply(&match result.code {
                ActionCode::Done => format!("Owner approved: calling @{handle}."),
                ActionCode::Unavailable => format!(
                    "Owner approved the call to @{handle}, but the agent cannot call now: {}.",
                    result.detail
                ),
                _ => format!(
                    "Owner approved the call to @{handle}, but it was not placed: {}.",
                    result.summary()
                ),
            });
        };
        let _ = reply.send(result);
    }

    /// The call to a contact is decided with `result` (when it still waits for one).
    pub(super) fn finish_contact_call(&mut self, result: impl FnOnce(&str) -> ActionResult) {
        if let Some(call) = self.contact_call.take() {
            let result = result(&call.handle);
            self.tell_caller(call, result);
        }
    }

    /// The phone placed the call to a contact: it rings unless the server refuses it
    /// meanwhile.
    pub(super) fn contact_call_placed(&mut self) {
        if let Some(call) = &mut self.contact_call {
            call.confirm_at = Some(Instant::now() + CONFIRM);
        }
    }

    /// The server refused the invite to a contact (`error`): the caller hears why, and the
    /// call is dropped without a note about a missed call.
    pub(super) fn contact_call_refused(&mut self, error: &SdkError) {
        tracing::info!(event = "call_refused_by_server", code = error.code());
        self.finish_contact_call(|_| ActionResult::from_sdk(error));
        if let Some(call) = &mut self.call {
            call.withdrawn = true;
            call.session.hang_up();
        }
    }

    /// The call to a contact rings: no refusal came in time.
    pub(super) fn confirm_contact_call(&mut self) {
        let due = self
            .contact_call
            .as_ref()
            .and_then(|call| call.confirm_at)
            .is_some_and(|at| at <= Instant::now());
        if due {
            self.finish_contact_call(ringing);
        }
    }

    /// Whether a call to a contact still waits for its result.
    pub(super) fn contact_call_undecided(&self) -> bool {
        self.contact_call.is_some()
    }

    /// The call to a contact ended before it rang for sure: the caller hears how.
    pub(super) fn contact_call_ended(&mut self, reason: CallEndReason) {
        self.finish_contact_call(|handle| not_rung(handle, reason));
    }

    /// The contact picked up: "Hi, this is <agent>, <owner>'s agent.", then the reason;
    /// the notice that the owner reads along goes into their chat once.
    pub(super) fn greet_contact(&mut self) {
        self.finish_contact_call(ringing);
        let Some(call) = &self.call else {
            return;
        };
        let Some(Purpose::Contact {
            peer, chat, reason, ..
        }) = &call.purpose
        else {
            return;
        };
        let agent = self.agent_name.as_deref().unwrap_or(&self.name);
        let hello = match &self.owner_display {
            Some(owner) => format!("Hi, this is {agent}, {owner}'s agent."),
            None => format!("Hi, this is {agent}, an agent."),
        };
        call.session.say(hello);
        if let Some(reason) = reason {
            for sentence in speech::sentences(reason) {
                call.session.say(sentence);
            }
        }
        // The notice is for the contact's chat; the owner's chat needs none.
        let (peer, conversation) = (*peer, chat.direct());
        if let Some(conversation) = conversation {
            if self.contacts.noticed.insert(peer) {
                let notice = self.contacts.notice.clone();
                self.contact_reply(conversation, &notice);
            }
        }
    }

    /// The contact on the call said `text`: posted in the call's chat, then to the
    /// backend as their message.
    pub(super) async fn contact_said(&mut self, peer: UserId, chat: ContactChat, text: String) {
        self.contact_heard(&chat, TRANSCRIPT_TITLE, &text);
        if let Some(call) = &mut self.call {
            call.unmute();
        }
        tracing::info!(event = "contact_prompt", call = true);
        self.prompt_contact(peer, chat, text).await;
    }

    /// The bridge stops: a call to a contact still undecided fails.
    pub(super) fn drop_contact_call(&mut self) {
        self.finish_contact_call(|_| ActionResult::failed("the bridge stopped"));
    }
}

/// The result of a call that rings.
fn ringing(handle: &str) -> ActionResult {
    ActionResult::done(format!("calling @{handle}"))
}

/// The result of a call to `handle` that ended (`reason`) before it surely rang.
fn not_rung(handle: &str, reason: CallEndReason) -> ActionResult {
    match reason {
        CallEndReason::Busy => {
            ActionResult::new(ActionCode::Unavailable, format!("@{handle} is busy"))
        }
        CallEndReason::Unreachable => ActionResult::new(
            ActionCode::Unavailable,
            format!("@{handle} cannot be reached now"),
        ),
        CallEndReason::Declined => ActionResult::failed(format!("@{handle} declined the call")),
        CallEndReason::Missed => ActionResult::failed(format!("@{handle} did not answer")),
        CallEndReason::Cancelled => ActionResult::failed("the call was cancelled"),
        _ => ActionResult::failed(format!("the call to @{handle} failed")),
    }
}

//! The agent's accepted contacts, for a backend that takes them
//! ([`crate::backend::Backend::accepts_contacts`]): their texts in their direct
//! conversation go to [`crate::backend::Backend::contact_prompt`], the backend's replies
//! and questions come back into that conversation, and only the contact who was asked
//! answers a question. Everything privileged stays the owner's: a contact may run only
//! `/help` and `/status`, and calls are the owner's alone.

use push_sdk::{
    ActivityState, ClientMsgId, ContactState, ConversationId, Message, OpenCard, UserId,
};
use tokio::time::{Duration, Instant};

use super::{Core, Internal};
use crate::backend::{Answer, BackendError, BackendRequest, ContactId, RequestKind, Responder};
use crate::cards::{self, CardKind, TypedOutcome};
use crate::commands::{self, Command};
use crate::messenger::Messenger;
use crate::render::{formatted_bodies, reply_bodies};

/// How long the server's answer about a relationship is trusted.
const CONTACT_FRESH: Duration = Duration::from_secs(60);
/// A contact's `/help`.
pub const CONTACT_HELP: &str = "\
/status: the agent's name and version
/help: this list
Anything else goes to the agent.";
/// A contact's command other than `/help` and `/status`.
pub const OWNER_ONLY: &str = "Only the agent's owner can use this command.";

/// A contact's open question.
pub(super) struct ContactRequest {
    /// Who was asked: the only one whose answer counts.
    user: UserId,
    conversation: ConversationId,
    kind: CardKind,
    responder: Option<Responder>,
    /// The card; `None` for a question shown as text only.
    card: Option<ClientMsgId>,
    deadline: Instant,
}

/// What the core knows about the agent's contacts.
#[derive(Default)]
pub(super) struct Contacts {
    /// Each user's relationship and when it was learned.
    states: std::collections::HashMap<UserId, (ContactState, Instant)>,
    /// Each contact's direct conversation with the agent.
    conversations: std::collections::HashMap<UserId, ConversationId>,
    /// The contacts' open questions, oldest first.
    pub(super) open: Vec<ContactRequest>,
    /// The activity shown in each contact's conversation (not idle), and when it was sent.
    activity: std::collections::HashMap<ConversationId, (ActivityState, Option<String>, Instant)>,
}

impl<M: Messenger> Core<M> {
    /// A text from someone other than the owner, with a backend that takes contacts:
    /// an accepted contact's text in their direct conversation reaches the backend;
    /// anyone else is a stranger.
    pub(super) async fn contact_message(&mut self, message: Message) {
        let (user, conversation) = (message.sender_id, message.conversation_id);
        if !self.is_contact(user).await {
            return self.stranger(user, conversation);
        }
        if self.contact_conversation(user).await != Some(conversation) {
            tracing::debug!(event = "contact_text_elsewhere");
            return;
        }
        if self.before_start(&message) {
            tracing::info!(event = "old_text_skipped");
            return;
        }
        if let Some(text) = message.body {
            self.contact_text(user, conversation, text).await;
        }
    }

    /// The server's word on `user`: an accepted contact (blocked users, requests either
    /// way and strangers are not).
    async fn is_contact(&mut self, user: UserId) -> bool {
        let now = Instant::now();
        if let Some((state, at)) = self.contacts.states.get(&user) {
            if now.saturating_duration_since(*at) < CONTACT_FRESH {
                return *state == ContactState::Accepted;
            }
        }
        let state = match self.messenger.contact_state(user).await {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(event = "contact_state_failed", code = error.code());
                return false;
            }
        };
        self.contacts.states.insert(user, (state, now));
        state == ContactState::Accepted
    }

    /// A relationship changed: what the server said is the latest word.
    pub(super) fn contact_changed(&mut self, user: UserId, state: ContactState) {
        self.contacts.states.insert(user, (state, Instant::now()));
    }

    async fn contact_conversation(&mut self, user: UserId) -> Option<ConversationId> {
        if let Some(conversation) = self.contacts.conversations.get(&user) {
            return Some(*conversation);
        }
        match self.messenger.direct(user).await {
            Ok(conversation) => {
                self.contacts.conversations.insert(user, conversation);
                Some(conversation)
            }
            Err(error) => {
                tracing::warn!(event = "contact_conversation_failed", code = error.code());
                None
            }
        }
    }

    /// A contact's text: `/help` and `/status` run, other commands are the owner's; with
    /// a question open the text answers it; else it goes to the backend.
    async fn contact_text(&mut self, user: UserId, conversation: ConversationId, text: String) {
        let text = match commands::parse(&text) {
            None | Some(Command::Passthrough(_)) => text,
            Some(command) => {
                tracing::info!(event = "contact_command", name = command.name());
                let reply = match command {
                    Command::Help => CONTACT_HELP.to_owned(),
                    Command::Status => self.contact_status().await,
                    _ => OWNER_ONLY.to_owned(),
                };
                return self.contact_reply(conversation, &reply);
            }
        };
        if let Some(index) = self
            .contacts
            .open
            .iter()
            .position(|request| request.user == user)
        {
            match cards::answer_from_text(&self.contacts.open[index].kind, &text) {
                TypedOutcome::Answer(answer) => {
                    let request = self.contacts.open.remove(index);
                    tracing::info!(event = "contact_card_answered_by_text");
                    self.settle_contact(request, answer).await;
                }
                TypedOutcome::Retry(reply) => self.contact_reply(conversation, reply),
            }
            return;
        }
        tracing::info!(event = "contact_prompt");
        let working = self.labels.working.clone();
        self.contact_activity(conversation, ActivityState::Working, Some(&working))
            .await;
        let backend = self.backend.clone();
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let result = backend
                .contact_prompt(ContactId(user.to_string()), &text)
                .await;
            let _ = internal.send(Internal::ContactPrompted {
                conversation,
                result,
            });
        });
    }

    /// `/status` for a contact: the backend's name and version, nothing about the
    /// owner's session or folders.
    async fn contact_status(&self) -> String {
        let version = match self.backend.status().await {
            Ok(status) => status.version,
            Err(_) => None,
        };
        let version = version
            .or_else(|| self.version.clone())
            .unwrap_or_else(|| "unknown".to_owned());
        format!("{}: {version}", self.name)
    }

    /// The backend is done with a contact's text.
    pub(super) async fn contact_prompted(
        &mut self,
        conversation: ConversationId,
        result: Result<(), BackendError>,
    ) {
        if let Err(error) = result {
            tracing::warn!(
                event = "contact_prompt_failed",
                method = error.method.as_str(),
                code = error.code.as_str()
            );
            self.contact_reply(conversation, &error.to_string());
        }
        if !self.contact_waiting(conversation) {
            self.contact_activity(conversation, ActivityState::Idle, None)
                .await;
        }
    }

    /// The contact the backend names, with their conversation.
    fn contact_of(&self, contact: &ContactId) -> Option<(UserId, ConversationId)> {
        self.contacts
            .conversations
            .iter()
            .find(|(user, _)| user.to_string() == contact.0)
            .map(|(user, conversation)| (*user, *conversation))
    }

    /// The backend's reply to a contact, formatted as its turns are.
    pub(super) fn contact_backend_reply(&self, contact: &ContactId, text: &str) {
        let Some((_, conversation)) = self.contact_of(contact) else {
            tracing::warn!(event = "contact_unknown");
            return;
        };
        for body in formatted_bodies(None, text, self.format) {
            self.send_to(conversation, &body);
        }
    }

    /// The backend asks a contact: a question card in their conversation that only they
    /// may answer. Anything but a question is declined.
    pub(super) async fn open_contact_request(
        &mut self,
        contact: &ContactId,
        request: BackendRequest,
    ) {
        let BackendRequest { kind, responder } = request;
        let Some((user, conversation)) = self.contact_of(contact) else {
            tracing::warn!(event = "contact_unknown");
            return responder.answer(Answer::Decline);
        };
        let RequestKind::Question {
            question,
            options,
            multi_select,
            allow_free_text,
        } = &kind
        else {
            tracing::warn!(event = "contact_request_refused", kind = ?kind);
            return responder.answer(Answer::Decline);
        };
        let plan = cards::question_card(question, options, *multi_select, *allow_free_text);
        tracing::info!(event = "contact_request_opened", kind = ?plan.kind);
        let card = match &plan.card {
            Some(body) => match self.messenger.send_rich(conversation, body) {
                Ok(id) => Some(id),
                Err(error) => {
                    tracing::warn!(event = "card_refused", code = error.code());
                    responder.answer(Answer::Decline);
                    return self.contact_reply(
                        conversation,
                        &format!(
                            "Could not show the card ({}); the request was declined.",
                            error.code()
                        ),
                    );
                }
            },
            None => None,
        };
        for body in &plan.follow_ups {
            self.send_to(conversation, body);
        }
        if let Some(id) = card {
            let timeout_ms = i64::try_from(self.answer_timeout.as_millis()).unwrap_or(i64::MAX);
            self.cards.open(OpenCard {
                conversation,
                client_msg_id: id,
                message_id: None,
                owner: user,
                offered: plan.offered.clone(),
                max_values: plan.max_values,
                deadline_ms: self.clock.now_ms().saturating_add(timeout_ms),
            });
        }
        self.contacts.open.push(ContactRequest {
            user,
            conversation,
            kind: plan.kind,
            responder: Some(responder),
            card,
            deadline: Instant::now() + self.answer_timeout,
        });
        let label = self.labels.question.clone();
        self.contact_activity(conversation, ActivityState::Waiting, Some(&label))
            .await;
    }

    /// A card answer the CardBook accepted (from the one who was asked) for a contact's
    /// card; `false` when `card` is not one.
    pub(super) async fn contact_card_answered(
        &mut self,
        card: ClientMsgId,
        values: &[String],
    ) -> bool {
        let Some(index) = self
            .contacts
            .open
            .iter()
            .position(|request| request.card == Some(card))
        else {
            return false;
        };
        let request = self.contacts.open.remove(index);
        let answer = cards::answer_from_values(&request.kind, values);
        tracing::info!(event = "contact_card_answered", values = values.len());
        self.settle_contact(request, answer).await;
        true
    }

    async fn settle_contact(&mut self, request: ContactRequest, answer: Answer) {
        if let Some(card) = request.card {
            self.cards.close(card);
        }
        if let Some(responder) = request.responder {
            responder.answer(answer);
        }
        let working = self.labels.working.clone();
        self.contact_activity(request.conversation, ActivityState::Working, Some(&working))
            .await;
    }

    /// The server refused a contact's card: its request is declined. `false` when `id`
    /// is not a contact's card.
    pub(super) fn contact_card_refused(&mut self, id: ClientMsgId, code: &str) -> bool {
        let Some(index) = self
            .contacts
            .open
            .iter()
            .position(|request| request.card == Some(id))
        else {
            return false;
        };
        let request = self.contacts.open.remove(index);
        self.cards.close(id);
        if let Some(responder) = request.responder {
            responder.answer(Answer::Decline);
        }
        self.contact_reply(
            request.conversation,
            &format!("Could not show the card ({code}); the request was declined."),
        );
        true
    }

    /// Contacts' questions past their deadline get their timeout answer and "No answer;
    /// skipped".
    pub(super) fn expire_contact_requests(&mut self) {
        let now = Instant::now();
        let (expired, open): (Vec<_>, Vec<_>) = std::mem::take(&mut self.contacts.open)
            .into_iter()
            .partition(|request| request.deadline <= now);
        self.contacts.open = open;
        for request in expired {
            tracing::info!(event = "contact_card_expired");
            if let Some(card) = request.card {
                self.cards.close(card);
            }
            if let (Some(responder), Some(answer)) =
                (request.responder, cards::timeout_answer(&request.kind))
            {
                responder.answer(answer);
            }
            self.contact_reply(request.conversation, "No answer; skipped");
        }
    }

    /// The bridge stops: the contacts' questions are declined, said once per
    /// conversation, and their activity goes idle.
    pub(super) async fn close_contact_requests(&mut self) {
        let open = std::mem::take(&mut self.contacts.open);
        let mut told: Vec<ConversationId> = Vec::new();
        for request in open {
            if let Some(card) = request.card {
                self.cards.close(card);
            }
            if !told.contains(&request.conversation) {
                told.push(request.conversation);
                self.contact_reply(request.conversation, "Declined: the bridge was stopped.");
            }
        }
        for conversation in told {
            self.contact_activity(conversation, ActivityState::Idle, None)
                .await;
        }
    }

    /// A contact's question is open in `conversation`.
    fn contact_waiting(&self, conversation: ConversationId) -> bool {
        self.contacts
            .open
            .iter()
            .any(|request| request.conversation == conversation)
    }

    /// The bridge's own words to a contact (`agent.reply@1`, masked).
    fn contact_reply(&self, conversation: ConversationId, text: &str) {
        for body in reply_bodies(None, text) {
            self.send_to(conversation, &body);
        }
    }

    fn send_to(&self, conversation: ConversationId, body: &str) {
        if let Err(error) = self.messenger.send_rich(conversation, body) {
            tracing::warn!(event = "send_failed", code = error.code());
        }
    }

    async fn contact_activity(
        &mut self,
        conversation: ConversationId,
        state: ActivityState,
        label: Option<&str>,
    ) {
        if state == ActivityState::Idle {
            self.contacts.activity.remove(&conversation);
        } else {
            self.contacts.activity.insert(
                conversation,
                (state, label.map(str::to_owned), Instant::now()),
            );
        }
        if let Err(error) = self.messenger.activity(conversation, state, label).await {
            tracing::debug!(event = "activity_failed", code = error.code());
        }
    }

    /// Sends the contacts' lasting activity again when due (others show it for 30 s), or
    /// at once with `force` (after a reconnect).
    pub(super) async fn refresh_contact_activity(&mut self, force: bool) {
        let now = Instant::now();
        let due: Vec<_> = self
            .contacts
            .activity
            .iter()
            .filter(|(_, (_, _, sent))| {
                force || now.saturating_duration_since(*sent) >= super::ACTIVITY_REFRESH
            })
            .map(|(conversation, (state, label, _))| (*conversation, *state, label.clone()))
            .collect();
        for (conversation, state, label) in due {
            self.contact_activity(conversation, state, label.as_deref())
                .await;
        }
    }
}

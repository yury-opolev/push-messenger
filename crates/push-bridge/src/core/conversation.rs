//! Messenger events: the owner gate, the owner's texts and card answers, sends the
//! server refused, and the connection.

use push_sdk::{
    CardCheck, ClientMsgId, ConnectionState, ConversationId, Event, Message, MessageKind, SdkError,
    UserId,
};
use tokio::time::Instant;

use super::{BridgeError, Core, Internal, PRIVATE, SEEN, STRANGER_QUIET, retry_delay};
use crate::backend::{Answer, BackendError, Capability, ErrorKind, SteerOutcome};
use crate::cards::{self, TypedOutcome};
use crate::commands;
use crate::mask::cap;
use crate::messenger::Messenger;

/// Characters of an undelivered text quoted back to the owner.
const UNDELIVERED_SHOWN: usize = 500;

impl<M: Messenger> Core<M> {
    pub(super) async fn on_messenger(&mut self, event: Event) -> Result<(), BridgeError> {
        match event {
            Event::MessageAdded(message) => self.on_message(message).await,
            Event::MessageSent(message) => self.cards.sent(&message),
            Event::SendFailed {
                client_msg_id,
                error,
                ..
            } => self.send_failed(client_msg_id, &error).await,
            Event::ConnectionStateChanged(state) => self.connection_changed(state).await?,
            Event::SignedOut => return Err(BridgeError::SignedOut),
            Event::DeviceRemoved => return Err(BridgeError::DeviceRemoved),
            Event::OpenedElsewhere => return Err(BridgeError::OpenedElsewhere),
            Event::EventsLost => tracing::warn!(event = "messenger_events_lost"),
            _ => {}
        }
        Ok(())
    }

    async fn connection_changed(&mut self, state: ConnectionState) -> Result<(), BridgeError> {
        let previous = self.connection.replace(state);
        match state {
            ConnectionState::SignedOut => Err(BridgeError::SignedOut),
            ConnectionState::DeviceRemoved => Err(BridgeError::DeviceRemoved),
            ConnectionState::OpenedElsewhere => Err(BridgeError::OpenedElsewhere),
            ConnectionState::Connected => {
                let reconnected =
                    previous.is_some_and(|previous| previous != ConnectionState::Connected);
                if reconnected {
                    tracing::info!(event = "messenger_reconnected");
                    self.update_activity(true).await;
                    self.fetch_pending().await;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn on_message(&mut self, message: Message) {
        if message.sender_id == self.messenger.me() || message.deleted {
            return;
        }
        if let Some(seq) = message.seq {
            if !self.first_delivery(message.conversation_id, seq) {
                return;
            }
        }
        match message.kind {
            MessageKind::Answer => self.on_answer(&message).await,
            MessageKind::Text | MessageKind::Rich if message.sender_id != self.owner => {
                self.stranger(message.sender_id, message.conversation_id);
            }
            MessageKind::Text if message.conversation_id == self.conversation => {
                if let Some(text) = message.body {
                    self.owner_text(text).await;
                }
            }
            _ => tracing::debug!(event = "message_ignored"),
        }
    }

    /// Remembers a delivered message; `false` when it was handled already.
    fn first_delivery(&mut self, conversation: ConversationId, seq: i64) -> bool {
        if !self.seen.insert((conversation, seq)) {
            return false;
        }
        self.seen_order.push_back((conversation, seq));
        if self.seen_order.len() > SEEN {
            if let Some(oldest) = self.seen_order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        true
    }

    /// Anyone but the owner hears [`PRIVATE`], at most once an hour; nothing reaches the
    /// backend.
    fn stranger(&mut self, sender: UserId, conversation: ConversationId) {
        let now = Instant::now();
        let quiet = |at: &Instant| now.saturating_duration_since(*at) < STRANGER_QUIET;
        if self.strangers.get(&sender).is_some_and(quiet) {
            return;
        }
        self.strangers.retain(|_, at| quiet(at));
        self.strangers.insert(sender, now);
        tracing::info!(event = "stranger_refused");
        if let Err(error) = self.messenger.send_text(conversation, PRIVATE) {
            tracing::warn!(event = "send_failed", code = error.code());
        }
    }

    async fn on_answer(&mut self, message: &Message) {
        match self.cards.check(message, self.clock.now_ms()) {
            CardCheck::Accepted { card, values } => {
                let Some(index) = self
                    .open
                    .iter()
                    .position(|request| request.card == Some(card.client_msg_id))
                else {
                    return;
                };
                let request = self.open.remove(index);
                let answer = cards::answer_from_values(&request.kind, &values);
                tracing::info!(event = "card_answered", values = values.len());
                self.settle(request, answer).await;
            }
            CardCheck::Refused(refusal) => {
                tracing::info!(event = "answer_refused", reason = ?refusal);
            }
        }
    }

    /// A command runs; else the text answers the oldest open request; else it goes to
    /// the agent.
    pub(super) async fn owner_text(&mut self, text: String) {
        if let Some(command) = commands::parse(&text) {
            tracing::info!(event = "command", name = command.name());
            self.command(command).await;
            return;
        }
        if let Some(request) = self.open.first() {
            match cards::answer_from_text(&request.kind, &text) {
                TypedOutcome::Answer(answer) => {
                    let request = self.open.remove(0);
                    tracing::info!(event = "card_answered_by_text");
                    self.settle(request, answer).await;
                }
                TypedOutcome::Retry(reply) => self.reply(reply),
            }
            return;
        }
        self.send_to_agent(text).await;
    }

    /// Steers the running turn (or waits for it to end without steering); waits for a
    /// slot-holding command; else prompts.
    pub(super) async fn send_to_agent(&mut self, text: String) {
        self.deliver(text, 0).await;
    }

    /// [`Self::send_to_agent`] for the `attempt`th time (from 0).
    pub(super) async fn deliver(&mut self, text: String, attempt: u32) {
        if self.turn.is_some() {
            if self.has(Capability::Steer) {
                self.steer(text, attempt).await;
            } else {
                self.queued.push(text);
            }
        } else if self.slot_commands > 0 {
            self.queued.push(text);
        } else {
            self.prompt(text, attempt);
        }
    }

    pub(super) fn prompt(&self, text: String, attempt: u32) {
        tracing::info!(event = "prompt", attempt);
        let backend = self.backend.clone();
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let result = backend.prompt(&text).await;
            let _ = internal.send(Internal::Prompted {
                text,
                attempt,
                result,
            });
        });
    }

    pub(super) async fn prompted(
        &mut self,
        text: String,
        attempt: u32,
        result: Result<(), BackendError>,
    ) {
        match result {
            Ok(()) => {}
            Err(error) if error.kind == ErrorKind::Busy => {
                if self.has(Capability::Steer) {
                    self.steer(text, attempt).await;
                } else {
                    self.wait_or_retry(text, attempt);
                }
            }
            Err(error) => self.backend_failed(&error),
        }
    }

    /// Steers; a turn that is ending (or none) gets the text as a prompt later.
    async fn steer(&mut self, text: String, attempt: u32) {
        tracing::info!(event = "steer");
        match self.backend.steer(&text).await {
            Ok(SteerOutcome::Accepted) => {}
            Ok(SteerOutcome::TurnEnding | SteerOutcome::NoActiveTurn) => {
                self.wait_or_retry(text, attempt);
            }
            Ok(SteerOutcome::Rejected(reason)) => {
                self.reply(&format!("{} did not take it: {reason}", self.name));
            }
            Err(error) => self.backend_failed(&error),
        }
    }

    /// The agent is busy: the text waits for the running turn (or slot-holding command)
    /// here, which sends it when it ends. With nothing here to wait for (Coda reports a
    /// turn's end before it is free again), it is prompted again after a pause, never
    /// at once; when the pauses are spent, the owner hears it was not delivered.
    fn wait_or_retry(&mut self, text: String, attempt: u32) {
        if self.turn.is_some() || self.slot_commands > 0 {
            self.queued.push(text);
            return;
        }
        let retry = attempt + 1;
        let Some(delay) = retry_delay(retry) else {
            tracing::warn!(event = "prompt_undelivered", attempts = retry);
            return self.reply(&format!(
                "{} stayed busy; this was not delivered, send it again:\n{}",
                self.name,
                cap(&text, UNDELIVERED_SHOWN)
            ));
        };
        tracing::info!(
            event = "prompt_retry_scheduled",
            retry,
            delay_ms = delay.as_millis() as u64
        );
        let internal = self.internal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = internal.send(Internal::Retry {
                text,
                attempt: retry,
            });
        });
    }

    /// Replies "<method> failed (<code>): <message>".
    pub(super) fn backend_failed(&self, error: &BackendError) {
        tracing::warn!(
            event = "backend_call_failed",
            method = error.method.as_str(),
            code = error.code.as_str()
        );
        self.reply(&error.to_string());
    }

    /// The server refused a card: its request is declined.
    async fn send_failed(&mut self, id: ClientMsgId, error: &SdkError) {
        tracing::warn!(event = "send_refused", code = error.code());
        let Some(index) = self
            .open
            .iter()
            .position(|request| request.card == Some(id))
        else {
            return;
        };
        let request = self.open.remove(index);
        self.cards.close(id);
        if let Some(responder) = request.responder {
            responder.answer(Answer::Decline);
        }
        self.reply(&format!(
            "Could not show the card ({}); the request was declined.",
            error.code()
        ));
        self.update_activity(false).await;
    }
}

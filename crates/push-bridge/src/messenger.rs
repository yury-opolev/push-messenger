//! The core's way to push-messenger: a small [`Messenger`] trait, the [`SdkMessenger`]
//! over a signed-in `push_sdk::Client`, and the [`TestMessenger`] tests drive the core with.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use push_domain::templates::{RichBody, builtins};
use push_sdk::{
    ActivityState, Client, ClientMsgId, ConversationId, Event, Events, Message, MessageId,
    MessageKind, MessageStatus, OutboxKind, SdkError, UserId,
};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};

/// Events waiting for the core, as the SDK's own queue.
pub const EVENT_QUEUE: usize = 10_000;

/// What the core needs from push-messenger.
#[async_trait::async_trait]
pub trait Messenger: Send + Sync + 'static {
    /// The agent's own user id.
    fn me(&self) -> UserId;
    /// The agent's owner, from its own profile (`owner_user_id`).
    async fn owner(&self) -> Result<UserId, SdkError>;
    /// The direct conversation with `peer`.
    async fn direct(&self, peer: UserId) -> Result<ConversationId, SdkError>;
    /// Queues a RICH body; the server may still refuse it (`Event::SendFailed`).
    fn send_rich(&self, conversation: ConversationId, body: &str) -> Result<ClientMsgId, SdkError>;
    /// Replaces the RICH body of the agent's message `id`.
    fn edit_rich(
        &self,
        conversation: ConversationId,
        id: ClientMsgId,
        body: &str,
    ) -> Result<(), SdkError>;
    fn send_text(&self, conversation: ConversationId, text: &str) -> Result<ClientMsgId, SdkError>;
    /// Live activity in `conversation`; fire and forget.
    async fn activity(
        &self,
        conversation: ConversationId,
        state: ActivityState,
        label: Option<&str>,
    ) -> Result<(), SdkError>;
}

/// A [`Messenger`] over a signed-in client.
pub struct SdkMessenger {
    client: Client,
    me: UserId,
}

impl SdkMessenger {
    /// `NotSignedIn` when the client has no signed-in account.
    pub fn new(client: Client) -> Result<Self, SdkError> {
        let me = client.account()?.ok_or(SdkError::NotSignedIn)?.user_id;
        Ok(Self { client, me })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Forwards the client's events into a channel the core reads.
    pub fn forward(mut events: Events) -> mpsc::Receiver<Event> {
        let (sender, receiver) = mpsc::channel(EVENT_QUEUE);
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                if sender.send(event).await.is_err() {
                    return;
                }
            }
        });
        receiver
    }
}

#[async_trait::async_trait]
impl Messenger for SdkMessenger {
    fn me(&self) -> UserId {
        self.me
    }

    async fn owner(&self) -> Result<UserId, SdkError> {
        let profiles = self.client.profiles(vec![self.me]).await?;
        profiles
            .into_iter()
            .find(|profile| profile.user_id == self.me)
            .and_then(|profile| profile.owner_user_id)
            .ok_or_else(|| SdkError::Invalid("this account is not an owned agent".to_owned()))
    }

    async fn direct(&self, peer: UserId) -> Result<ConversationId, SdkError> {
        Ok(self.client.open_direct(peer).await?.id)
    }

    fn send_rich(&self, conversation: ConversationId, body: &str) -> Result<ClientMsgId, SdkError> {
        Ok(self.client.send_rich(conversation, body)?.client_msg_id)
    }

    fn edit_rich(
        &self,
        conversation: ConversationId,
        id: ClientMsgId,
        body: &str,
    ) -> Result<(), SdkError> {
        self.client.edit(conversation, id, body).map(drop)
    }

    fn send_text(&self, conversation: ConversationId, text: &str) -> Result<ClientMsgId, SdkError> {
        Ok(self.client.send(conversation, text)?.client_msg_id)
    }

    async fn activity(
        &self,
        conversation: ConversationId,
        state: ActivityState,
        label: Option<&str>,
    ) -> Result<(), SdkError> {
        self.client.send_activity(conversation, state, label).await
    }
}

/// One call the core made on a [`TestMessenger`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    SendRich {
        conversation: ConversationId,
        id: ClientMsgId,
        body: String,
    },
    EditRich {
        conversation: ConversationId,
        id: ClientMsgId,
        body: String,
    },
    SendText {
        conversation: ConversationId,
        id: ClientMsgId,
        text: String,
    },
    Activity {
        conversation: ConversationId,
        state: ActivityState,
        label: Option<String>,
    },
}

#[derive(Default)]
struct Recorded {
    calls: Vec<Call>,
    /// Peer → direct conversation.
    conversations: HashMap<UserId, ConversationId>,
    /// The server ids of the agent's stored messages.
    stored: HashMap<ClientMsgId, MessageId>,
    next_seq: i64,
    /// The next RICH sends the server refuses, with these errors.
    refusals: Vec<SdkError>,
}

/// A [`Messenger`] for tests: records every call, plays the server (each send is stored
/// at once and reported as `MessageSent`, unless [`TestMessenger::fail_next_rich`]) and
/// injects the owner's and strangers' messages into the core's event channel.
pub struct TestMessenger {
    me: UserId,
    owner: UserId,
    events: mpsc::Sender<Event>,
    recorded: Mutex<Recorded>,
    changed: Notify,
}

impl TestMessenger {
    /// The messenger and the event channel to hand to the core.
    pub fn new() -> (Arc<Self>, mpsc::Receiver<Event>) {
        let (events, receiver) = mpsc::channel(EVENT_QUEUE);
        let messenger = Self {
            me: UserId::new(),
            owner: UserId::new(),
            events,
            recorded: Mutex::new(Recorded::default()),
            changed: Notify::new(),
        };
        (Arc::new(messenger), receiver)
    }

    pub fn owner_id(&self) -> UserId {
        self.owner
    }

    /// The direct conversation with `peer` (created on first use).
    pub fn conversation_with(&self, peer: UserId) -> ConversationId {
        *self.lock().conversations.entry(peer).or_default()
    }

    /// The direct conversation with the owner.
    pub fn conversation(&self) -> ConversationId {
        self.conversation_with(self.owner)
    }

    /// The next RICH send is refused by the server with `error` (`Event::SendFailed`).
    pub fn fail_next_rich(&self, error: SdkError) {
        self.lock().refusals.push(error);
    }

    /// Delivers `event` to the core.
    pub fn inject(&self, event: Event) {
        self.events
            .try_send(event)
            .expect("the core reads its events");
    }

    /// A message delivered from `sender` in `conversation`, with the next `seq`.
    pub fn message(
        &self,
        conversation: ConversationId,
        sender: UserId,
        kind: MessageKind,
        body: &str,
    ) -> Message {
        let seq = self.next_seq();
        Message {
            conversation_id: conversation,
            client_msg_id: ClientMsgId::new(),
            message_id: Some(MessageId::new()),
            seq: Some(seq),
            sender_id: sender,
            kind,
            body: Some(body.to_owned()),
            client_ts: None,
            server_ts: Some(seq),
            edited_at: None,
            deleted: false,
            status: MessageStatus::Received,
            answer_seq: None,
            card_seq: None,
        }
    }

    /// The owner writes `text` to the agent; returns the delivered message.
    pub fn owner_says(&self, text: &str) -> Message {
        self.says(self.owner, text)
    }

    /// `sender` writes `text` in their direct conversation with the agent.
    pub fn says(&self, sender: UserId, text: &str) -> Message {
        let conversation = self.conversation_with(sender);
        let message = self.message(conversation, sender, MessageKind::Text, text);
        self.inject(Event::MessageAdded(message.clone()));
        message
    }

    /// `sender` answers the agent's card `card` (a `ClientMsgId` the core sent) with
    /// `values`, in the owner's conversation.
    pub fn answer_as(&self, sender: UserId, card: ClientMsgId, values: &[&str]) -> Message {
        let target = self
            .lock()
            .stored
            .get(&card)
            .copied()
            .expect("the card was stored");
        let body = json!({"card": target.to_string(), "values": values, "fallback": "answer"});
        let mut message = self.message(
            self.conversation(),
            sender,
            MessageKind::Answer,
            &body.to_string(),
        );
        message.card_seq = Some(1);
        self.inject(Event::MessageAdded(message.clone()));
        message
    }

    /// The owner answers card `card` with `values`.
    pub fn answer(&self, card: ClientMsgId, values: &[&str]) -> Message {
        self.answer_as(self.owner, card, values)
    }

    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    /// The RICH bodies sent (not edits), in order: `(id, body)`.
    pub fn sent_rich(&self) -> Vec<(ClientMsgId, String)> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::SendRich { id, body, .. } => Some((id, body)),
                _ => None,
            })
            .collect()
    }

    /// The edits made: `(id, body)`.
    pub fn edits(&self) -> Vec<(ClientMsgId, String)> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::EditRich { id, body, .. } => Some((id, body)),
                _ => None,
            })
            .collect()
    }

    /// The plain texts sent: `(conversation, text)`.
    pub fn texts(&self) -> Vec<(ConversationId, String)> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::SendText {
                    conversation, text, ..
                } => Some((conversation, text)),
                _ => None,
            })
            .collect()
    }

    /// The activity sent, with repeats of the same state and label folded.
    pub fn activities(&self) -> Vec<(ActivityState, Option<String>)> {
        let mut shown: Vec<(ActivityState, Option<String>)> = Vec::new();
        for call in self.calls() {
            if let Call::Activity { state, label, .. } = call {
                if shown.last() != Some(&(state, label.clone())) {
                    shown.push((state, label));
                }
            }
        }
        shown
    }

    /// The `text` of every `agent.reply@1` sent (not edits), in order.
    pub fn reply_texts(&self) -> Vec<String> {
        self.sent_rich()
            .iter()
            .filter(|(_, body)| template(body) == "agent.reply@1")
            .filter_map(|(_, body)| param(body, "text"))
            .collect()
    }

    /// Waits until `done` holds for the calls so far; panics after 60 s.
    pub async fn wait_until(&self, what: &str, done: impl Fn(&[Call]) -> bool) {
        wait(&self.changed, what, || done(&self.calls())).await;
    }

    fn record(&self, call: Call) {
        self.lock().calls.push(call);
        self.changed.notify_waiters();
    }

    fn next_seq(&self) -> i64 {
        let mut recorded = self.lock();
        recorded.next_seq += 1;
        recorded.next_seq
    }

    /// Plays the server for one of the agent's sends: its local echo, then stored or
    /// refused.
    fn stored(
        &self,
        conversation: ConversationId,
        id: ClientMsgId,
        kind: MessageKind,
        body: &str,
        refusal: Option<SdkError>,
    ) {
        let mut message = self.message(conversation, self.me, kind, body);
        message.client_msg_id = id;
        message.status = MessageStatus::Pending;
        let (seq, message_id) = (message.seq, message.message_id);
        message.seq = None;
        message.message_id = None;
        self.inject(Event::MessageAdded(message.clone()));
        match refusal {
            Some(error) => self.inject(Event::SendFailed {
                conversation_id: conversation,
                client_msg_id: id,
                kind: OutboxKind::Send,
                error,
            }),
            None => {
                message.seq = seq;
                message.message_id = message_id;
                message.status = MessageStatus::Sent;
                if let Some(message_id) = message_id {
                    self.lock().stored.insert(id, message_id);
                }
                self.inject(Event::MessageSent(message));
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Recorded> {
        self.recorded.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait::async_trait]
impl Messenger for TestMessenger {
    fn me(&self) -> UserId {
        self.me
    }

    async fn owner(&self) -> Result<UserId, SdkError> {
        Ok(self.owner)
    }

    async fn direct(&self, peer: UserId) -> Result<ConversationId, SdkError> {
        Ok(self.conversation_with(peer))
    }

    fn send_rich(&self, conversation: ConversationId, body: &str) -> Result<ClientMsgId, SdkError> {
        check_rich(body)?;
        let id = ClientMsgId::new();
        self.record(Call::SendRich {
            conversation,
            id,
            body: body.to_owned(),
        });
        let refusal = {
            let mut recorded = self.lock();
            (!recorded.refusals.is_empty()).then(|| recorded.refusals.remove(0))
        };
        self.stored(conversation, id, MessageKind::Rich, body, refusal);
        Ok(id)
    }

    fn edit_rich(
        &self,
        conversation: ConversationId,
        id: ClientMsgId,
        body: &str,
    ) -> Result<(), SdkError> {
        check_rich(body)?;
        self.record(Call::EditRich {
            conversation,
            id,
            body: body.to_owned(),
        });
        Ok(())
    }

    fn send_text(&self, conversation: ConversationId, text: &str) -> Result<ClientMsgId, SdkError> {
        let id = ClientMsgId::new();
        self.record(Call::SendText {
            conversation,
            id,
            text: text.to_owned(),
        });
        self.stored(conversation, id, MessageKind::Text, text, None);
        Ok(id)
    }

    async fn activity(
        &self,
        conversation: ConversationId,
        state: ActivityState,
        label: Option<&str>,
    ) -> Result<(), SdkError> {
        self.record(Call::Activity {
            conversation,
            state,
            label: label.map(str::to_owned),
        });
        Ok(())
    }
}

/// As the server does: a RICH body must fit its (built-in) template.
fn check_rich(body: &str) -> Result<(), SdkError> {
    let invalid = |error: String| SdkError::Invalid(error);
    let parsed = RichBody::parse(body.as_bytes()).map_err(|error| invalid(error.to_string()))?;
    let definition = builtins()
        .into_iter()
        .find(|definition| definition.template_ref() == parsed.template)
        .ok_or_else(|| invalid("not a built-in template".to_owned()))?;
    parsed
        .check(&definition)
        .map_err(|error| invalid(error.to_string()))
}

/// The template a RICH body names, such as `agent.reply@1`.
pub fn template(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get("tpl").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default()
}

/// A string parameter of a RICH body.
pub fn param(body: &str, name: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .pointer(&format!("/params/{name}"))?
        .as_str()
        .map(str::to_owned)
}

/// Waits until `done()` holds, rechecking whenever `changed` is notified; panics after 60 s.
pub(crate) async fn wait(changed: &Notify, what: &str, done: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let notified = changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if done() {
            return;
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            panic!("timed out waiting for {what}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLY: &str = r#"{"tpl":"agent.reply@1","params":{"text":"hi"},"fallback":"hi"}"#;

    #[tokio::test]
    async fn sends_are_echoed_then_stored() {
        let (messenger, mut events) = TestMessenger::new();
        let conversation = messenger.conversation();
        let id = messenger.send_rich(conversation, REPLY).unwrap();
        let Some(Event::MessageAdded(echo)) = events.recv().await else {
            panic!("the local echo");
        };
        assert_eq!((echo.client_msg_id, echo.sender_id), (id, messenger.me()));
        assert_eq!(echo.seq, None);
        let Some(Event::MessageSent(sent)) = events.recv().await else {
            panic!("stored");
        };
        assert_eq!(sent.client_msg_id, id);
        assert!(sent.seq.is_some() && sent.message_id.is_some());
        assert_eq!(messenger.reply_texts(), ["hi"]);
    }

    #[tokio::test]
    async fn a_refused_rich_send_fails() {
        let (messenger, mut events) = TestMessenger::new();
        messenger.fail_next_rich(SdkError::Invalid("no".into()));
        let id = messenger
            .send_rich(messenger.conversation(), REPLY)
            .unwrap();
        events.recv().await;
        let Some(Event::SendFailed {
            client_msg_id,
            error,
            ..
        }) = events.recv().await
        else {
            panic!("refused");
        };
        assert_eq!((client_msg_id, error.code()), (id, "invalid"));
    }

    #[test]
    fn bodies_that_do_not_fit_their_template_are_invalid() {
        let (messenger, _events) = TestMessenger::new();
        let unknown = r#"{"tpl":"agent.nope@1","params":{},"fallback":"x"}"#;
        assert!(
            messenger
                .send_rich(messenger.conversation(), unknown)
                .is_err()
        );
        let missing = r#"{"tpl":"agent.approval@1","params":{},"fallback":"x"}"#;
        assert!(
            messenger
                .send_rich(messenger.conversation(), missing)
                .is_err()
        );
        assert!(messenger.calls().is_empty());
    }

    #[tokio::test]
    async fn answers_name_the_stored_card() {
        let (messenger, mut events) = TestMessenger::new();
        let id = messenger
            .send_rich(messenger.conversation(), REPLY)
            .unwrap();
        events.recv().await;
        let Some(Event::MessageSent(card)) = events.recv().await else {
            panic!("stored");
        };
        let answer = messenger.answer(id, &["approve"]);
        let body: Value = serde_json::from_str(answer.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["card"], card.message_id.unwrap().to_string());
        assert_eq!(answer.sender_id, messenger.owner_id());
        assert_eq!(answer.kind, MessageKind::Answer);
    }

    #[test]
    fn activities_fold_repeats() {
        let (messenger, _events) = TestMessenger::new();
        for (state, label) in [
            (ActivityState::Working, Some("thinking")),
            (ActivityState::Working, Some("thinking")),
            (ActivityState::Idle, None),
        ] {
            messenger.record(Call::Activity {
                conversation: messenger.conversation(),
                state,
                label: label.map(str::to_owned),
            });
        }
        assert_eq!(
            messenger.activities(),
            [
                (ActivityState::Working, Some("thinking".to_owned())),
                (ActivityState::Idle, None)
            ]
        );
    }
}

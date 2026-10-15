//! Sending (spec §4.2, §8.3, §8.4): messages, edits and deletes are checked, stored in
//! the outbox with their `client_msg_id` and sent once connected, in order; read positions
//! move at once and are sent too.

use push_domain::{ClientMsgId, ConversationId, UserId, validate_text};

use super::{Client, EDIT_WINDOW};
use crate::SdkError;
use crate::connection::{Command, unix_ms};
use crate::model::{Event, Message, MessageKind, OutboxKind};

impl Client {
    fn me(&self) -> Result<UserId, SdkError> {
        self.inner
            .shared
            .store()
            .owner()?
            .ok_or(SdkError::NotSignedIn)
    }

    fn wake(&self) {
        let _ = self.inner.commands.send(Command::Wake);
    }

    /// Queues a text message (spec §4.2, §8.3): checked first, stored in the outbox with
    /// a new `client_msg_id` before it is sent, and sent once connected, in order. The
    /// local echo is returned and reported as [`Event::MessageAdded`].
    pub fn send(&self, conversation: ConversationId, text: &str) -> Result<Message, SdkError> {
        validate_text(text.as_bytes()).map_err(|error| SdkError::Invalid(error.to_string()))?;
        self.me()?;
        let message = {
            let mut store = self.inner.shared.store();
            if store.conversation(conversation)?.is_none() {
                return Err(SdkError::Invalid("no such conversation".to_owned()));
            }
            store.queue_send(conversation, ClientMsgId::new(), text, unix_ms() * 1000)?
        };
        self.inner.shared.emit(Event::MessageAdded(message.clone()));
        self.wake();
        Ok(message)
    }

    /// The user's own message `target`, checked for an edit or delete (spec §8.3).
    fn own_target(
        &self,
        conversation: ConversationId,
        target: ClientMsgId,
    ) -> Result<Message, SdkError> {
        let me = self.me()?;
        let message = self
            .inner
            .shared
            .store()
            .message(conversation, target)?
            .ok_or_else(|| SdkError::Invalid("no such message".to_owned()))?;
        if message.sender_id != me {
            return Err(SdkError::Invalid("not the user's own message".to_owned()));
        }
        if message.deleted {
            return Err(SdkError::Invalid("the message is deleted".to_owned()));
        }
        if message.kind != MessageKind::Text {
            return Err(SdkError::Invalid("not a text message".to_owned()));
        }
        let window = i64::try_from(EDIT_WINDOW.as_micros()).unwrap_or(i64::MAX);
        if message
            .server_ts
            .is_some_and(|sent| unix_ms() * 1000 - sent > window)
        {
            return Err(SdkError::Invalid(
                "the time to change it has passed".to_owned(),
            ));
        }
        Ok(message)
    }

    /// Changes the text of one of the user's messages. A message still waiting in the
    /// outbox is changed in place; otherwise the edit is queued (after the message, if it
    /// is being sent) and applied locally when the server acknowledges it. The id of the
    /// outbox entry that carries the change.
    pub fn edit(
        &self,
        conversation: ConversationId,
        target: ClientMsgId,
        text: &str,
    ) -> Result<ClientMsgId, SdkError> {
        validate_text(text.as_bytes()).map_err(|error| SdkError::Invalid(error.to_string()))?;
        let message = self.own_target(conversation, target)?;
        let mut parent = None;
        if message.seq.is_none() {
            let edited = self.inner.shared.store().edit_unsent(target, text)?;
            if edited {
                let message = self.inner.shared.store().message(conversation, target)?;
                if let Some(message) = message {
                    self.inner.shared.emit(Event::MessageEdited(message));
                }
                return Ok(target);
            }
            parent = Some(target);
        }
        let id = ClientMsgId::new();
        self.inner.shared.store().queue_change(
            conversation,
            id,
            OutboxKind::Edit,
            &message,
            Some(text),
            parent,
            unix_ms() * 1000,
        )?;
        self.wake();
        Ok(id)
    }

    /// Deletes one of the user's messages: a message still waiting in the outbox is
    /// dropped; otherwise the delete is queued and the body erased locally when the
    /// server acknowledges it. The id of the outbox entry that carries the change.
    pub fn delete(
        &self,
        conversation: ConversationId,
        target: ClientMsgId,
    ) -> Result<ClientMsgId, SdkError> {
        let message = self.own_target(conversation, target)?;
        let mut parent = None;
        if message.seq.is_none() {
            if self.inner.shared.store().drop_unsent(target)? {
                self.inner.shared.emit(Event::MessageDeleted(Message {
                    body: None,
                    deleted: true,
                    ..message
                }));
                return Ok(target);
            }
            parent = Some(target);
        }
        let id = ClientMsgId::new();
        self.inner.shared.store().queue_change(
            conversation,
            id,
            OutboxKind::Delete,
            &message,
            None,
            parent,
            unix_ms() * 1000,
        )?;
        self.wake();
        Ok(id)
    }

    /// Takes back a message that has not been sent yet (spec §8.2); false once it was
    /// sent at least once, when the server may have stored it.
    pub fn cancel(
        &self,
        conversation: ConversationId,
        client_msg_id: ClientMsgId,
    ) -> Result<bool, SdkError> {
        let message = self
            .inner
            .shared
            .store()
            .message(conversation, client_msg_id)?;
        let Some(message) = message else {
            return Ok(false);
        };
        if !self.inner.shared.store().drop_unsent(client_msg_id)? {
            return Ok(false);
        }
        self.inner.shared.emit(Event::MessageDeleted(Message {
            body: None,
            deleted: true,
            ..message
        }));
        Ok(true)
    }

    /// The user read everything up to `seq` (spec §4.5): the unread count drops at once
    /// and the position is sent once connected. Positions only move forward, and never
    /// past the conversation's last message the client knows.
    pub fn mark_read(&self, conversation: ConversationId, seq: i64) -> Result<(), SdkError> {
        let changed = self.inner.shared.store().mark_read(conversation, seq)?;
        if let Some(conversation) = changed {
            self.inner
                .shared
                .emit(Event::ConversationChanged(conversation));
            self.wake();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use push_proto::v1::{self, EntryKind, frame::Body};
    use uuid::Uuid;

    use super::*;
    use crate::client::tests::{Fixture, open};
    use crate::model::MessageStatus;
    use crate::store::bytes;
    use crate::store::tests::entry;
    use crate::testing::{recv_until, send};

    fn invalid<T>(result: Result<T, SdkError>) -> bool {
        matches!(result, Err(SdkError::Invalid(_)))
    }

    #[tokio::test]
    async fn nothing_invalid_is_queued() {
        let mut f = Fixture::new().await;
        let (conversation, peer) = f.conversation();
        assert!(invalid(f.client.send(conversation, "")));
        assert!(invalid(
            f.client.send(conversation, &"x".repeat(64 * 1024 + 1))
        ));
        assert!(invalid(f.client.send(ConversationId::new(), "hi")));
        assert!(f.client.inner.shared.store().outbox().unwrap().is_empty());
        assert!(f.events().is_empty());

        // Someone else's message cannot be edited or deleted.
        let theirs = entry(conversation, peer, 1, EntryKind::Message);
        f.client
            .inner
            .shared
            .store()
            .apply_deliver(&theirs)
            .unwrap();
        let theirs = ClientMsgId(Uuid::from_slice(&theirs.client_msg_id).unwrap());
        assert!(invalid(f.client.edit(conversation, theirs, "mine now")));
        assert!(invalid(f.client.delete(conversation, theirs)));

        // Nor one of the user's own after the 48 hours.
        let old = f.client.send(conversation, "old").unwrap();
        let sent_long_ago = unix_ms() * 1000 - i64::try_from(EDIT_WINDOW.as_micros()).unwrap() - 1;
        let ack = v1::SendAck {
            client_msg_id: bytes(old.client_msg_id.0),
            message_id: bytes(Uuid::now_v7()),
            seq: 2,
            server_ts: sent_long_ago,
        };
        f.client.inner.shared.store().acknowledged(&ack).unwrap();
        assert!(invalid(f.client.edit(
            conversation,
            old.client_msg_id,
            "too late"
        )));
    }

    #[tokio::test]
    async fn an_unsent_message_is_changed_in_place_or_taken_back() {
        let mut f = Fixture::new().await;
        let (conversation, _) = f.conversation();
        let message = f.client.send(conversation, "helo").unwrap();
        assert_eq!(message.status, MessageStatus::Pending);
        let id = message.client_msg_id;
        assert_eq!(f.client.edit(conversation, id, "hello").unwrap(), id);
        let second = f.client.send(conversation, "oops").unwrap();
        assert!(f.client.cancel(conversation, second.client_msg_id).unwrap());
        let events = f.events();
        assert!(matches!(&events[..], [
            Event::MessageAdded(_),
            Event::MessageEdited(edited),
            Event::MessageAdded(_),
            Event::MessageDeleted(cancelled),
        ] if edited.body.as_deref() == Some("hello") && cancelled.deleted));
        let outbox = f.client.inner.shared.store().outbox().unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].body.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn the_outbox_survives_a_restart() {
        let f = Fixture::new().await;
        // Holds the shared in-memory database open between the two clients.
        let _keep = rusqlite::Connection::open(&f.database).unwrap();
        let (conversation, _) = f.conversation();
        let queued = f.client.send(conversation, "sent after a restart").unwrap();
        let Fixture {
            client,
            events,
            auth,
            gateway,
            secrets,
            database,
        } = f;
        drop((client, events));

        let (client, events) = open(&auth, &gateway, &database, &secrets);
        let mut f = Fixture {
            client,
            events,
            auth,
            gateway,
            secrets,
            database,
        };
        f.client.connect().unwrap();
        let mut socket = f.ready().await;
        let Body::Send(sent) = recv_until(&mut socket, |body| matches!(body, Body::Send(_))).await
        else {
            unreachable!();
        };
        assert_eq!(sent.client_msg_id, bytes(queued.client_msg_id.0));
        let ack = v1::SendAck {
            client_msg_id: sent.client_msg_id,
            message_id: bytes(Uuid::now_v7()),
            seq: 1,
            server_ts: unix_ms() * 1000,
        };
        send(&mut socket, Body::SendAck(ack)).await;
        let sent = loop {
            match tokio::time::timeout(Duration::from_secs(10), f.events.next()).await {
                Ok(Some(Event::MessageSent(message))) => break message,
                Ok(Some(_)) => {}
                other => panic!("no MessageSent: {other:?}"),
            }
        };
        assert_eq!(
            (sent.client_msg_id, sent.seq),
            (queued.client_msg_id, Some(1))
        );
    }
}

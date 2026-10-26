//! The outbox (spec §8.4): sends, edits and deletes waiting for their `SendAck`, sent in
//! order per conversation, one at a time, with the same `client_msg_id` on every attempt.

use push_domain::{ClientMsgId, ConversationId, MessageId};
use push_proto::v1;
use rusqlite::{OptionalExtension, Row, params};
use uuid::Uuid;

use super::{Store, bytes, id_of, uuid_at};
use crate::SdkError;
use crate::model::{Message, MessageKind, OutboxKind, redacted};

/// A row of the outbox.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OutboxRow {
    pub client_msg_id: ClientMsgId,
    pub conversation_id: ConversationId,
    pub kind: OutboxKind,
    pub target_message_id: Option<MessageId>,
    pub target_client_msg_id: Option<ClientMsgId>,
    /// What a send carries; an edit or delete carries its target's kind.
    pub content: MessageKind,
    pub body: Option<String>,
    pub in_flight: bool,
    pub enqueued_at: i64,
    pub attempts: u32,
    pub parent_id: Option<ClientMsgId>,
}

impl std::fmt::Debug for OutboxRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutboxRow")
            .field("client_msg_id", &self.client_msg_id)
            .field("conversation_id", &self.conversation_id)
            .field("kind", &self.kind)
            .field("target_message_id", &self.target_message_id)
            .field("target_client_msg_id", &self.target_client_msg_id)
            .field("content", &self.content)
            .field("body", &redacted(&self.body))
            .field("in_flight", &self.in_flight)
            .field("enqueued_at", &self.enqueued_at)
            .field("attempts", &self.attempts)
            .field("parent_id", &self.parent_id)
            .finish()
    }
}

impl OutboxRow {
    /// The frame that sends this row.
    pub fn frame(&self) -> v1::frame::Body {
        let conversation_id = bytes(self.conversation_id.0);
        let client_msg_id = bytes(self.client_msg_id.0);
        match self.kind {
            OutboxKind::Send => v1::frame::Body::Send(v1::Send {
                client_msg_id,
                conversation_id,
                content_type: self.content.content_type().into(),
                body: self.body.clone().unwrap_or_default().into_bytes(),
                client_ts: Some(self.enqueued_at),
            }),
            OutboxKind::Edit => v1::frame::Body::Edit(v1::Edit {
                client_msg_id,
                conversation_id,
                target: Some(match (self.target_message_id, self.target_client_msg_id) {
                    (Some(id), _) => v1::edit::Target::TargetMessageId(bytes(id.0)),
                    (None, Some(id)) => v1::edit::Target::TargetClientMsgId(bytes(id.0)),
                    (None, None) => v1::edit::Target::TargetMessageId(Vec::new()),
                }),
                body: self.body.clone().unwrap_or_default().into_bytes(),
                client_ts: Some(self.enqueued_at),
            }),
            OutboxKind::Delete => v1::frame::Body::Delete(v1::Delete {
                client_msg_id,
                conversation_id,
                target: Some(match (self.target_message_id, self.target_client_msg_id) {
                    (Some(id), _) => v1::delete::Target::TargetMessageId(bytes(id.0)),
                    (None, Some(id)) => v1::delete::Target::TargetClientMsgId(bytes(id.0)),
                    (None, None) => v1::delete::Target::TargetMessageId(Vec::new()),
                }),
            }),
        }
    }
}

impl Store {
    /// Queues a text message and stores its local echo, in one transaction.
    #[cfg(test)]
    pub fn queue_send(
        &mut self,
        id: ConversationId,
        client_msg_id: ClientMsgId,
        body: &str,
        now_us: i64,
    ) -> Result<Message, SdkError> {
        self.queue_content(id, client_msg_id, MessageKind::Text, body, None, now_us)
    }

    /// Queues a message of `kind` (an answer names its card's `seq`) and stores its local
    /// echo, in one transaction.
    pub fn queue_content(
        &mut self,
        id: ConversationId,
        client_msg_id: ClientMsgId,
        kind: MessageKind,
        body: &str,
        card_seq: Option<i64>,
        now_us: i64,
    ) -> Result<Message, SdkError> {
        let me = self.me()?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO outbox (client_msg_id, conversation_id, kind, content_type, body, enqueued_at)
             VALUES (?1, ?2, 1, ?3, ?4, ?5)",
            params![
                bytes(client_msg_id.0),
                bytes(id.0),
                kind.code(),
                body.as_bytes(),
                now_us
            ],
        )?;
        tx.execute(
            "INSERT INTO message (conversation_id, client_msg_id, sender_id, content_type, body,
                 client_ts, card_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                bytes(id.0),
                bytes(client_msg_id.0),
                bytes(me.0),
                kind.code(),
                body.as_bytes(),
                now_us,
                card_seq
            ],
        )?;
        tx.commit()?;
        Ok(self.message(id, client_msg_id)?.expect("just queued"))
    }

    /// Queues an edit or a delete of `target`, after `parent` when the target is still in
    /// the outbox.
    #[allow(clippy::too_many_arguments)]
    pub fn queue_change(
        &mut self,
        id: ConversationId,
        client_msg_id: ClientMsgId,
        kind: OutboxKind,
        target: &Message,
        body: Option<&str>,
        parent: Option<ClientMsgId>,
        now_us: i64,
    ) -> Result<(), SdkError> {
        self.conn.execute(
            "INSERT INTO outbox (client_msg_id, conversation_id, kind, target_message_id,
                 target_client_msg_id, content_type, body, enqueued_at, parent_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?9, ?6, ?7, ?8)",
            params![
                bytes(client_msg_id.0),
                bytes(id.0),
                kind.code(),
                target.message_id.map(|id| bytes(id.0)),
                bytes(target.client_msg_id.0),
                body.map(str::as_bytes),
                now_us,
                parent.map(|id| bytes(id.0)),
                target.kind.code(),
            ],
        )?;
        Ok(())
    }

    pub fn outbox_row(&self, client_msg_id: ClientMsgId) -> Result<Option<OutboxRow>, SdkError> {
        Ok(self
            .conn
            .query_row(
                &format!("{OUTBOX_COLUMNS} WHERE client_msg_id = ?1"),
                params![bytes(client_msg_id.0)],
                outbox_row,
            )
            .optional()?)
    }

    /// Every row, in sending order.
    pub fn outbox(&self) -> Result<Vec<OutboxRow>, SdkError> {
        let mut statement = self
            .conn
            .prepare(&format!("{OUTBOX_COLUMNS} ORDER BY rowid"))?;
        let rows = statement.query_map([], outbox_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// An unsent queued message gets a new body in place (spec §8.4); false if it was
    /// sent at least once already.
    pub fn edit_unsent(
        &mut self,
        client_msg_id: ClientMsgId,
        body: &str,
    ) -> Result<bool, SdkError> {
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "UPDATE outbox SET body = ?2 WHERE client_msg_id = ?1 AND kind = 1 AND attempts = 0 AND state = 0",
            params![bytes(client_msg_id.0), body.as_bytes()],
        )?;
        if changed > 0 {
            tx.execute(
                "UPDATE message SET body = ?2 WHERE client_msg_id = ?1 AND seq IS NULL",
                params![bytes(client_msg_id.0), body.as_bytes()],
            )?;
        }
        tx.commit()?;
        Ok(changed > 0)
    }

    /// An unsent queued message is dropped with its local echo (spec §8.4: cancel, or a
    /// delete before sending); false if it was sent at least once already.
    pub fn drop_unsent(&mut self, client_msg_id: ClientMsgId) -> Result<bool, SdkError> {
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "DELETE FROM outbox WHERE client_msg_id = ?1 AND attempts = 0 AND state = 0",
            params![bytes(client_msg_id.0)],
        )?;
        if changed > 0 {
            tx.execute(
                "DELETE FROM message WHERE client_msg_id = ?1 AND seq IS NULL",
                params![bytes(client_msg_id.0)],
            )?;
        }
        tx.commit()?;
        Ok(changed > 0)
    }

    /// The rows to send now: per conversation the first row, if it is queued and due.
    pub fn due(&self, now_ms: i64) -> Result<Vec<OutboxRow>, SdkError> {
        let mut statement = self.conn.prepare(&format!(
            "{OUTBOX_COLUMNS} WHERE rowid IN (SELECT MIN(rowid) FROM outbox GROUP BY conversation_id)
                 AND state = 0 AND next_attempt_at <= ?1 ORDER BY rowid"
        ))?;
        let rows = statement.query_map(params![now_ms], outbox_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// When the next row waiting for a retry is due, in milliseconds since the epoch.
    pub fn next_due(&self) -> Result<Option<i64>, SdkError> {
        Ok(self.conn.query_row(
            "SELECT MIN(next_attempt_at) FROM outbox
             WHERE rowid IN (SELECT MIN(rowid) FROM outbox GROUP BY conversation_id) AND state = 0",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn mark_in_flight(&mut self, client_msg_id: ClientMsgId) -> Result<(), SdkError> {
        self.conn.execute(
            "UPDATE outbox SET state = 1, attempts = attempts + 1 WHERE client_msg_id = ?1",
            params![bytes(client_msg_id.0)],
        )?;
        Ok(())
    }

    /// The connection dropped: rows in flight are queued again, to be sent again with the
    /// same `client_msg_id`.
    pub fn requeue_in_flight(&mut self) -> Result<(), SdkError> {
        self.conn
            .execute("UPDATE outbox SET state = 0 WHERE state = 1", [])?;
        Ok(())
    }

    pub fn retry_at(&mut self, client_msg_id: ClientMsgId, at_ms: i64) -> Result<(), SdkError> {
        self.conn.execute(
            "UPDATE outbox SET state = 0, next_attempt_at = ?2 WHERE client_msg_id = ?1",
            params![bytes(client_msg_id.0), at_ms],
        )?;
        Ok(())
    }

    /// The server stored the row: it leaves the outbox, and a message's local echo gets
    /// its server id and position. The row, if it was still there.
    pub fn acknowledged(&mut self, ack: &v1::SendAck) -> Result<Option<OutboxRow>, SdkError> {
        let client_msg_id = ClientMsgId(id_of(&ack.client_msg_id)?);
        let Some(row) = self.outbox_row(client_msg_id)? else {
            return Ok(None);
        };
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM outbox WHERE client_msg_id = ?1",
            params![bytes(client_msg_id.0)],
        )?;
        if row.kind == OutboxKind::Send {
            tx.execute(
                "UPDATE message SET seq = COALESCE(seq, ?3), message_id = ?4, server_ts = ?5
                 WHERE conversation_id = ?1 AND client_msg_id = ?2",
                params![
                    bytes(row.conversation_id.0),
                    bytes(client_msg_id.0),
                    ack.seq,
                    ack.message_id,
                    ack.server_ts
                ],
            )?;
            tx.execute(
                "UPDATE conversation SET last_seq = MAX(last_seq, ?2), updated_at = MAX(updated_at, ?3)
                 WHERE id = ?1",
                params![bytes(row.conversation_id.0), ack.seq, ack.server_ts],
            )?;
        }
        tx.commit()?;
        Ok(Some(row))
    }

    /// The server refused the row for good: it leaves the outbox with a message's local
    /// echo, and so do the rows that change the refused message. Every row removed.
    pub fn refused(&mut self, client_msg_id: ClientMsgId) -> Result<Vec<OutboxRow>, SdkError> {
        let Some(row) = self.outbox_row(client_msg_id)? else {
            return Ok(Vec::new());
        };
        let mut removed = vec![row.clone()];
        if row.kind == OutboxKind::Send {
            removed.extend(self.outbox()?.into_iter().filter(|other| {
                other.target_client_msg_id == Some(client_msg_id)
                    && other.client_msg_id != client_msg_id
            }));
        }
        let tx = self.conn.transaction()?;
        for gone in &removed {
            tx.execute(
                "DELETE FROM outbox WHERE client_msg_id = ?1",
                params![bytes(gone.client_msg_id.0)],
            )?;
        }
        if row.kind == OutboxKind::Send {
            tx.execute(
                "DELETE FROM message WHERE conversation_id = ?1 AND client_msg_id = ?2 AND seq IS NULL",
                params![bytes(row.conversation_id.0), bytes(client_msg_id.0)],
            )?;
        }
        tx.commit()?;
        Ok(removed)
    }
}

const OUTBOX_COLUMNS: &str = "SELECT client_msg_id, conversation_id, kind, target_message_id,
    target_client_msg_id, body, state, enqueued_at, attempts, parent_id, content_type FROM outbox";

fn outbox_row(row: &Row<'_>) -> rusqlite::Result<OutboxRow> {
    let optional_id = |index: usize| -> rusqlite::Result<Option<Uuid>> {
        let value: Option<Vec<u8>> = row.get(index)?;
        Ok(value.and_then(|value| Uuid::from_slice(&value).ok()))
    };
    let body: Option<Vec<u8>> = row.get(5)?;
    let state: i64 = row.get(6)?;
    let attempts: i64 = row.get(8)?;
    Ok(OutboxRow {
        client_msg_id: ClientMsgId(uuid_at(row, 0)?),
        conversation_id: ConversationId(uuid_at(row, 1)?),
        kind: OutboxKind::from_code(row.get(2)?),
        target_message_id: optional_id(3)?.map(MessageId),
        target_client_msg_id: optional_id(4)?.map(ClientMsgId),
        content: MessageKind::from_code(row.get::<_, Option<i64>>(10)?.unwrap_or(1)),
        body: body.map(|body| String::from_utf8_lossy(&body).into_owned()),
        in_flight: state == 1,
        enqueued_at: row.get(7)?,
        attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
        parent_id: optional_id(9)?.map(ClientMsgId),
    })
}

#[cfg(test)]
mod tests {
    use push_domain::UserId;
    use push_proto::v1::EntryKind;

    use super::*;
    use crate::model::MessageStatus;
    use crate::store::Applied;
    use crate::store::tests::{Fixture, entry, fixture};

    #[test]
    fn the_outbox_sends_one_row_per_conversation_in_order() {
        let mut f = fixture();
        let other = ConversationId::new();
        f.store.open_direct(other, UserId::new()).unwrap();
        let (a, b, c) = (ClientMsgId::new(), ClientMsgId::new(), ClientMsgId::new());
        f.store.queue_send(f.conversation, a, "a", 1).unwrap();
        f.store.queue_send(f.conversation, b, "b", 2).unwrap();
        f.store.queue_send(other, c, "c", 3).unwrap();
        let due: Vec<_> = f
            .store
            .due(0)
            .unwrap()
            .iter()
            .map(|row| row.client_msg_id)
            .collect();
        assert_eq!(due, [a, c]);
        f.store.mark_in_flight(a).unwrap();
        let due: Vec<_> = f
            .store
            .due(0)
            .unwrap()
            .iter()
            .map(|row| row.client_msg_id)
            .collect();
        assert_eq!(due, [c], "b waits for a");
        // A dropped connection queues a again, to be sent with the same id.
        f.store.requeue_in_flight().unwrap();
        let row = f.store.outbox_row(a).unwrap().unwrap();
        assert_eq!((row.in_flight, row.attempts), (false, 1));
        // A retry later holds the conversation back until it is due.
        f.store.retry_at(a, 5_000).unwrap();
        let due: Vec<_> = f
            .store
            .due(4_999)
            .unwrap()
            .iter()
            .map(|row| row.client_msg_id)
            .collect();
        assert_eq!(due, [c]);
        assert_eq!(f.store.next_due().unwrap(), Some(0));
        assert_eq!(f.store.due(5_000).unwrap()[0].client_msg_id, a);
    }

    #[test]
    fn an_unsent_message_is_edited_in_place_or_dropped_but_a_sent_one_is_not() {
        let mut f = fixture();
        let (a, b) = (ClientMsgId::new(), ClientMsgId::new());
        f.store.queue_send(f.conversation, a, "first", 1).unwrap();
        f.store.queue_send(f.conversation, b, "second", 2).unwrap();
        assert!(f.store.edit_unsent(a, "first, fixed").unwrap());
        assert_eq!(
            f.store
                .message(f.conversation, a)
                .unwrap()
                .unwrap()
                .body
                .as_deref(),
            Some("first, fixed")
        );
        let row = f.store.outbox_row(a).unwrap().unwrap();
        assert!(!format!("{row:?}").contains("fixed"), "Debug shows no body");
        assert!(f.store.drop_unsent(b).unwrap());
        assert!(f.store.message(f.conversation, b).unwrap().is_none());
        f.store.mark_in_flight(a).unwrap();
        assert!(!f.store.edit_unsent(a, "too late").unwrap());
        assert!(!f.store.drop_unsent(a).unwrap());
    }

    #[test]
    fn a_refused_message_takes_its_changes_and_its_echo_along() {
        let mut f = fixture();
        let original = ClientMsgId::new();
        let message = f
            .store
            .queue_send(f.conversation, original, "hi", 1)
            .unwrap();
        f.store.mark_in_flight(original).unwrap();
        let edit = ClientMsgId::new();
        f.store
            .queue_change(
                f.conversation,
                edit,
                OutboxKind::Edit,
                &message,
                Some("hi!"),
                Some(original),
                2,
            )
            .unwrap();
        let removed: Vec<_> = f
            .store
            .refused(original)
            .unwrap()
            .iter()
            .map(|row| row.client_msg_id)
            .collect();
        assert_eq!(removed, [original, edit]);
        assert!(f.store.outbox().unwrap().is_empty());
        assert!(f.store.message(f.conversation, original).unwrap().is_none());
    }

    #[test]
    fn receipts_keep_the_highest_positions_and_drive_the_status() {
        let mut f = fixture();
        let client_msg_id = ClientMsgId::new();
        let queued = f
            .store
            .queue_send(f.conversation, client_msg_id, "hi", 10)
            .unwrap();
        assert_eq!(queued.status, MessageStatus::Pending);
        let ack = v1::SendAck {
            client_msg_id: bytes(client_msg_id.0),
            message_id: bytes(Uuid::now_v7()),
            seq: 1,
            server_ts: 20,
        };
        assert!(f.store.acknowledged(&ack).unwrap().is_some());
        assert!(f.store.acknowledged(&ack).unwrap().is_none());
        let status = |f: &Fixture| {
            f.store
                .message(f.conversation, client_msg_id)
                .unwrap()
                .unwrap()
                .status
        };
        assert_eq!(status(&f), MessageStatus::Sent);
        assert!(
            f.store
                .apply_receipt(f.conversation, 1, 0)
                .unwrap()
                .is_some()
        );
        assert_eq!(status(&f), MessageStatus::Delivered);
        assert!(
            f.store
                .apply_receipt(f.conversation, 1, 1)
                .unwrap()
                .is_some()
        );
        assert_eq!(status(&f), MessageStatus::Read);
        // A late, lower receipt moves nothing.
        assert!(
            f.store
                .apply_receipt(f.conversation, 0, 0)
                .unwrap()
                .is_none()
        );
        assert_eq!(status(&f), MessageStatus::Read);
        assert_eq!(
            f.store
                .message(f.conversation, client_msg_id)
                .unwrap()
                .unwrap()
                .sender_id,
            f.me
        );
    }

    #[test]
    fn the_own_message_delivered_back_is_confirmed_not_added() {
        let mut f = fixture();
        let client_msg_id = ClientMsgId::new();
        f.store
            .queue_send(f.conversation, client_msg_id, "mine", 1)
            .unwrap();
        let mut echo = entry(f.conversation, f.me, 1, EntryKind::Message);
        echo.client_msg_id = bytes(client_msg_id.0);
        let (applied, conversation, _) = f.store.apply_deliver(&echo).unwrap();
        assert!(matches!(applied, Applied::Confirmed(ref message) if message.seq == Some(1)));
        assert_eq!(
            conversation.unwrap().unread,
            0,
            "the user's own message is not unread"
        );
    }

    #[test]
    fn another_user_gets_an_empty_database() {
        let mut f = fixture();
        f.store
            .queue_send(f.conversation, ClientMsgId::new(), "hi", 1)
            .unwrap();
        assert!(!f.store.bind_owner(f.me).unwrap());
        assert!(f.store.bind_owner(UserId::new()).unwrap());
        assert!(f.store.outbox().unwrap().is_empty());
        assert!(f.store.conversations().unwrap().is_empty());
    }
}

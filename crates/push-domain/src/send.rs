use chrono::{DateTime, Utc};

use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::conversations::require_conversation;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
use crate::model::{ContentType, ConversationKind, Entry, EntryKind, SendReceipt};
use crate::notify::{Event, Notifier};
use crate::service::{Messaging, current_member_ids, require_device};
use crate::store::{InsertOutcome, Store, Tx};
use crate::validation::validate_text;

/// A text message to append. `sender` and `device` come from the verified token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendText {
    pub sender: UserId,
    pub device: DeviceId,
    pub conversation: ConversationId,
    /// Chosen by the client before the first attempt and reused on every retry.
    pub client_msg_id: ClientMsgId,
    pub body: Vec<u8>,
    pub client_ts: Option<DateTime<Utc>>,
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Appends a text message. A retry with the same `client_msg_id` returns the original
    /// receipt, stores nothing new and publishes nothing — even if the sender has since lost
    /// the right to send.
    pub async fn send_text(&self, command: SendText) -> DomainResult<SendReceipt> {
        validate_text(&command.body)?;

        let mut tx = self.store.begin().await?;
        if require_device(&mut tx, command.device).await? != command.sender {
            return Err(DomainError::UnknownDevice);
        }
        let conversation = require_conversation(&mut tx, command.conversation).await?;
        let now = self.clock.now();
        let seq = tx.next_seq(conversation.id, now).await?;

        // A non-member reusing an existing `client_msg_id` gets `ClientIdConflict`; that is
        // acceptable because ids are random and chosen by the client.
        // A retry of an entry that is already stored always gets its original receipt,
        // even if the sender has since lost the right to send. Checked under the
        // conversation lock taken by `next_seq`, so it cannot race a concurrent attempt.
        if let Some(original) = tx
            .entry_by_client_id(conversation.id, command.client_msg_id)
            .await?
        {
            tx.rollback().await?;
            return receipt_for(&original, command.sender);
        }

        let members = tx.members(conversation.id).await?;
        if !members
            .iter()
            .any(|member| member.user_id == command.sender && member.is_current())
        {
            return Err(DomainError::NotMember);
        }
        if conversation.kind == ConversationKind::Direct {
            for peer in members
                .iter()
                .filter(|member| member.user_id != command.sender)
            {
                if !are_contacts(&mut tx, command.sender, peer.user_id).await? {
                    return Err(DomainError::NotContact);
                }
            }
        }

        let entry = Entry {
            conversation_id: conversation.id,
            seq,
            message_id: MessageId::new(),
            client_msg_id: command.client_msg_id,
            kind: EntryKind::Message,
            target_seq: None,
            sender_id: command.sender,
            sender_device_id: Some(command.device),
            content_type: ContentType::Text,
            body: Some(command.body),
            client_ts: command.client_ts,
            server_ts: now,
            edited_at: None,
            edit_seq: None,
            deleted_at: None,
        };
        let receipt = SendReceipt::from(&entry);

        if tx.insert_entry(entry).await? == InsertOutcome::Duplicate {
            // Backstop: the lookup above normally catches retries. Rolling back returns
            // `seq` to the counter, keeping the log gapless.
            tx.rollback().await?;
            return self
                .replay_send(command.conversation, command.client_msg_id, command.sender)
                .await;
        }
        tx.raise_read_seq(conversation.id, command.sender, seq)
            .await?;
        tx.commit().await?;

        self.publish(vec![Event::EntriesAvailable {
            users: current_member_ids(&members),
        }])
        .await;
        Ok(receipt)
    }

    /// The receipt of an already stored entry, if it belongs to the same sender.
    async fn replay_send(
        &self,
        conversation: ConversationId,
        client_msg_id: ClientMsgId,
        sender: UserId,
    ) -> DomainResult<SendReceipt> {
        let mut tx = self.store.begin().await?;
        let original = tx
            .entry_by_client_id(conversation, client_msg_id)
            .await?
            .ok_or_else(|| {
                DomainError::Store("duplicate entry disappeared after a conflict".into())
            })?;
        tx.rollback().await?;
        receipt_for(&original, sender)
    }
}

/// The receipt of a stored entry, if it belongs to `sender`.
fn receipt_for(original: &Entry, sender: UserId) -> DomainResult<SendReceipt> {
    if original.sender_id != sender {
        return Err(DomainError::ClientIdConflict);
    }
    Ok(SendReceipt::from(original))
}

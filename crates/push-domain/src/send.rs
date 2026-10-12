use chrono::{DateTime, Utc};

use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::conversations::require_conversation;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
use crate::model::{
    ContentType, Conversation, ConversationKind, Entry, EntryKind, Member, SendReceipt,
};
use crate::notify::{Event, Notifier};
use crate::read::direct_receipt;
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

/// An open transaction holding the conversation lock, allowed to append one entry at `seq`.
pub(crate) struct Append<T> {
    pub(crate) tx: T,
    pub(crate) conversation: Conversation,
    /// Every member row, current and former.
    pub(crate) members: Vec<Member>,
    /// The sender's member row.
    pub(crate) member: Member,
    pub(crate) seq: i64,
    pub(crate) now: DateTime<Utc>,
}

pub(crate) enum Prepared<T> {
    /// The `client_msg_id` is already stored: this is its original receipt.
    Retry(SendReceipt),
    Ready(Append<T>),
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
    ///
    /// Advances the sender's read position to what the sending device has already received
    /// (its delivery position): replying marks as read everything the user could have seen,
    /// never messages their device has not received yet. When it moves, the sender's
    /// devices learn the new position, and if it passes over a message from the other
    /// member of a direct conversation, they get a read receipt.
    pub async fn send_text(&self, command: SendText) -> DomainResult<SendReceipt> {
        validate_text(&command.body)?;
        let prepared = self
            .prepare_append(
                command.sender,
                command.device,
                command.conversation,
                command.client_msg_id,
                EntryKind::Message,
            )
            .await?;
        let Append {
            mut tx,
            conversation,
            members,
            member,
            seq,
            now,
        } = match prepared {
            Prepared::Retry(receipt) => return Ok(receipt),
            Prepared::Ready(append) => append,
        };

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
            // Backstop: the lookup in `prepare_append` normally catches retries. Rolling back
            // returns `seq` to the counter, keeping the log gapless.
            tx.rollback().await?;
            return self
                .replay_append(
                    conversation.id,
                    command.client_msg_id,
                    command.sender,
                    EntryKind::Message,
                )
                .await;
        }
        let device_position = tx
            .get_cursor(command.device, conversation.id)
            .await?
            .unwrap_or(0);
        let read_moved = tx
            .raise_read_seq(conversation.id, command.sender, device_position)
            .await?;
        let read_receipt = direct_receipt(
            &mut tx,
            &conversation,
            command.sender,
            member.read_seq.max(member.visible_from_seq - 1),
            device_position,
        )
        .await?;
        tx.commit().await?;

        let mut events = vec![Event::EntriesAvailable {
            users: current_member_ids(&members),
        }];
        if read_moved {
            events.push(Event::ReadPositionChanged {
                user: command.sender,
                conversation: conversation.id,
                read_seq: device_position,
            });
        }
        events.extend(read_receipt);
        self.publish(events).await;
        Ok(receipt)
    }

    /// The common start of every append (spec §4.2): checks the device, locks the
    /// conversation and takes the next `seq`, answers a retry with its original receipt,
    /// then checks that the sender may write.
    pub(crate) async fn prepare_append(
        &self,
        sender: UserId,
        device: DeviceId,
        conversation: ConversationId,
        client_msg_id: ClientMsgId,
        kind: EntryKind,
    ) -> DomainResult<Prepared<S::Tx>> {
        let mut tx = self.store.begin().await?;
        if require_device(&mut tx, device).await? != sender {
            return Err(DomainError::UnknownDevice);
        }
        let conversation = require_conversation(&mut tx, conversation).await?;
        let now = self.clock.now();
        let seq = tx.next_seq(conversation.id, now).await?;

        // A non-member reusing an existing `client_msg_id` gets `ClientIdConflict`; that is
        // acceptable because ids are random and chosen by the client.
        // A retry of an entry that is already stored always gets its original receipt,
        // even if the sender has since lost the right to send. Checked under the
        // conversation lock taken by `next_seq`, so it cannot race a concurrent attempt.
        if let Some(original) = tx
            .entry_by_client_id(conversation.id, client_msg_id)
            .await?
        {
            tx.rollback().await?;
            return receipt_for(&original, sender, kind).map(Prepared::Retry);
        }

        let members = tx.members(conversation.id).await?;
        let member = members
            .iter()
            .find(|member| member.user_id == sender && member.is_current())
            .cloned()
            .ok_or(DomainError::NotMember)?;
        if conversation.kind == ConversationKind::Direct {
            for peer in members.iter().filter(|member| member.user_id != sender) {
                if !are_contacts(&mut tx, sender, peer.user_id).await? {
                    return Err(DomainError::NotContact);
                }
            }
        }
        Ok(Prepared::Ready(Append {
            tx,
            conversation,
            members,
            member,
            seq,
            now,
        }))
    }

    /// The receipt of an already stored entry, if it is the same sender's entry of the
    /// same kind.
    pub(crate) async fn replay_append(
        &self,
        conversation: ConversationId,
        client_msg_id: ClientMsgId,
        sender: UserId,
        kind: EntryKind,
    ) -> DomainResult<SendReceipt> {
        let mut tx = self.store.begin().await?;
        let original = tx
            .entry_by_client_id(conversation, client_msg_id)
            .await?
            .ok_or_else(|| {
                DomainError::Store("duplicate entry disappeared after a conflict".into())
            })?;
        tx.rollback().await?;
        receipt_for(&original, sender, kind)
    }
}

/// The receipt of a stored entry, if `sender` stored it as an entry of `kind`.
fn receipt_for(original: &Entry, sender: UserId, kind: EntryKind) -> DomainResult<SendReceipt> {
    if original.sender_id != sender || original.kind != kind {
        return Err(DomainError::ClientIdConflict);
    }
    Ok(SendReceipt::from(original))
}

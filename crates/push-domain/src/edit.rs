use chrono::{DateTime, TimeDelta, Utc};

use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
use crate::model::{ContentType, Entry, EntryKind, Member, SendReceipt};
use crate::notify::{Event, Notifier};
use crate::send::{Append, Prepared};
use crate::service::{Messaging, current_member_ids};
use crate::store::{InsertOutcome, Store, Tx};
use crate::validation::validate_text;

/// The message an edit or delete changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRef {
    /// The id the server assigned.
    Id(MessageId),
    /// The sender's `client_msg_id`, for when the client has not learned the server id yet.
    ClientId(ClientMsgId),
}

/// Replaces the text of a message. `sender` and `device` come from the verified token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditText {
    pub sender: UserId,
    pub device: DeviceId,
    pub conversation: ConversationId,
    /// The edit's own idempotency key, reused on every retry.
    pub client_msg_id: ClientMsgId,
    pub target: MessageRef,
    pub body: Vec<u8>,
    pub client_ts: Option<DateTime<Utc>>,
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Replaces the text of one of the sender's own messages (spec §4.6).
    ///
    /// The edit is a log entry of its own. In the same transaction the message row takes
    /// the new text, and the text of any earlier edit entry is erased, so only the current
    /// version is stored. A retry with the same `client_msg_id` returns the original
    /// receipt, before any permission or window check.
    pub async fn edit_text(&self, command: EditText) -> DomainResult<SendReceipt> {
        validate_text(&command.body)?;
        let prepared = self
            .prepare_append(
                command.sender,
                command.device,
                command.conversation,
                command.client_msg_id,
                EntryKind::Edit,
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
        let mut target = resolve_target(&mut tx, &member, command.target).await?;
        check_change(&target, command.sender, now, self.policy.edit_window)?;

        // Erase older edits first, so the only edit text left is the one inserted below.
        tx.erase_edit_bodies(conversation.id, target.seq).await?;
        let entry = Entry {
            conversation_id: conversation.id,
            seq,
            message_id: MessageId::new(),
            client_msg_id: command.client_msg_id,
            kind: EntryKind::Edit,
            target_seq: Some(target.seq),
            sender_id: command.sender,
            sender_device_id: Some(command.device),
            content_type: ContentType::Text,
            body: Some(command.body.clone()),
            client_ts: command.client_ts,
            server_ts: now,
            edited_at: None,
            edit_seq: None,
            deleted_at: None,
        };
        let receipt = SendReceipt::from(&entry);
        if tx.insert_entry(entry).await? == InsertOutcome::Duplicate {
            tx.rollback().await?;
            return self
                .replay_append(
                    conversation.id,
                    command.client_msg_id,
                    command.sender,
                    EntryKind::Edit,
                )
                .await;
        }
        target.body = Some(command.body);
        target.edited_at = Some(now);
        target.edit_seq = Some(seq);
        tx.update_entry(target).await?;
        tx.commit().await?;

        self.publish(vec![Event::EntriesAvailable {
            users: current_member_ids(&members),
        }])
        .await;
        Ok(receipt)
    }
}

/// The message `target` refers to, if it is a message the caller may see.
async fn resolve_target<T: Tx>(
    tx: &mut T,
    caller: &Member,
    target: MessageRef,
) -> DomainResult<Entry> {
    let conversation = caller.conversation_id;
    let found = match target {
        MessageRef::Id(id) => tx.entry_by_message_id(conversation, id).await?,
        MessageRef::ClientId(id) => tx.entry_by_client_id(conversation, id).await?,
    };
    found
        .filter(|entry| entry.kind == EntryKind::Message && entry.seq >= caller.visible_from_seq)
        .ok_or(DomainError::TargetNotFound)
}

/// Whether `sender` may still change `target` at `now`.
fn check_change(
    target: &Entry,
    sender: UserId,
    now: DateTime<Utc>,
    window: TimeDelta,
) -> DomainResult<()> {
    if target.sender_id != sender {
        return Err(DomainError::NotSender);
    }
    if target.deleted_at.is_some() {
        return Err(DomainError::AlreadyDeleted);
    }
    if now - target.server_ts > window {
        return Err(DomainError::WindowExpired);
    }
    Ok(())
}

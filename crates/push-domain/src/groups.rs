//! Groups of up to 100 members with admin and member roles (spec §6).

use chrono::{DateTime, Utc};

use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ClientMsgId, ConversationId, MessageId, UserId};
use crate::model::{ContentType, Conversation, ConversationKind, Entry, EntryKind, Member, Role};
use crate::notify::{Event, Notifier};
use crate::service::Messaging;
use crate::store::{Store, Tx};
use crate::system::SystemEvent;
use crate::validation::{MAX_GROUP_MEMBERS, validate_title};

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Creates a group titled `title` (trimmed) with `creator` as its admin and `members`
    /// as members. Every other member must be an accepted contact of the creator.
    /// Duplicates, and the creator in `members`, are ignored. A group has at most
    /// `MAX_GROUP_MEMBERS` members, the creator included.
    ///
    /// The log starts with a `Created` system entry at `seq` 1. Every member sees the group
    /// from there, and every active device of every member starts just before it.
    pub async fn create_group(
        &self,
        creator: UserId,
        title: &str,
        members: &[UserId],
    ) -> DomainResult<ConversationId> {
        let title = validate_title(title)?;
        let mut others: Vec<UserId> = members
            .iter()
            .copied()
            .filter(|member| *member != creator)
            .collect();
        others.sort();
        others.dedup();
        if others.len() + 1 > MAX_GROUP_MEMBERS {
            return Err(DomainError::GroupFull);
        }

        let mut tx = self.store.begin().await?;
        if !tx.user_exists(creator).await? {
            return Err(DomainError::UnknownUser);
        }
        for other in &others {
            if !are_contacts(&mut tx, creator, *other).await? {
                return Err(DomainError::NotContact);
            }
        }
        let now = self.clock.now();
        let conversation = Conversation {
            id: ConversationId::new(),
            kind: ConversationKind::Group,
            direct_key: None,
            title: Some(title.clone()),
            created_by: creator,
            last_seq: 0,
            created_at: now,
            updated_at: now,
        };
        let id = conversation.id;
        // A group has no direct key, so the insert cannot be a duplicate.
        tx.insert_conversation(conversation).await?;
        let seq = tx.next_seq(id, now).await?;
        let created = SystemEvent::Created {
            actor: creator,
            title,
            users: others.clone(),
        };
        tx.insert_entry(system_entry(id, seq, &created, now))
            .await?;
        join(&mut tx, id, creator, Role::Admin, seq).await?;
        for other in &others {
            join(&mut tx, id, *other, Role::Member, seq).await?;
        }
        tx.commit().await?;

        let mut users = others;
        users.push(creator);
        users.sort();
        self.publish(vec![Event::EntriesAvailable { users }]).await;
        Ok(id)
    }
}

/// Makes `user` a current member who sees the log from `seq`, the entry that let them in.
/// Nothing before it is visible or unread: the read position starts at `seq - 1`, and so
/// does every active device's delivery position. A device that already has a position (a
/// member rejoining) keeps it; readers never look below `visible_from_seq - 1`.
async fn join<T: Tx>(
    tx: &mut T,
    conversation: ConversationId,
    user: UserId,
    role: Role,
    seq: i64,
) -> DomainResult<()> {
    tx.insert_member(Member {
        conversation_id: conversation,
        user_id: user,
        role,
        visible_from_seq: seq,
        left_at_seq: None,
        read_seq: seq - 1,
    })
    .await?;
    for device in tx.devices_of_user(user).await? {
        tx.create_cursor_if_absent(device, conversation, seq - 1)
            .await?;
    }
    Ok(())
}

/// The log entry recording `event` at `seq`. Its sender is the event's actor; no device
/// sent it.
fn system_entry(
    conversation: ConversationId,
    seq: i64,
    event: &SystemEvent,
    now: DateTime<Utc>,
) -> Entry {
    Entry {
        conversation_id: conversation,
        seq,
        message_id: MessageId::new(),
        client_msg_id: ClientMsgId::new(),
        kind: EntryKind::System,
        target_seq: None,
        sender_id: event.actor(),
        sender_device_id: None,
        content_type: ContentType::SystemEvent,
        body: Some(event.encode()),
        client_ts: None,
        server_ts: now,
        edited_at: None,
        edit_seq: None,
        deleted_at: None,
    }
}

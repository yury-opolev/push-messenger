//! Groups of up to 100 members with admin and member roles (spec §6).

use chrono::{DateTime, Utc};

use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::conversations::require_conversation;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ClientMsgId, ConversationId, MessageId, UserId};
use crate::model::{ContentType, Conversation, ConversationKind, Entry, EntryKind, Member, Role};
use crate::notify::{Event, Notifier};
use crate::read::{GroupPositions, group_positions, group_receipt};
use crate::service::{Messaging, current_member_ids};
use crate::store::{Store, Tx};
use crate::system::SystemEvent;
use crate::validation::{MAX_GROUP_MEMBERS, ValidationError, validate_title};

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

    /// Adds accepted contacts of `admin` to a group as members. Users who are already
    /// current members are skipped, so adding twice changes nothing; former members
    /// rejoin. A group has at most `MAX_GROUP_MEMBERS` current members.
    ///
    /// A `MembersAdded` entry records the change. The added members see the log from that
    /// entry on, with nothing before it unread, and every active device of theirs starts
    /// just before it.
    pub async fn add_members(
        &self,
        admin: UserId,
        conversation: ConversationId,
        users: &[UserId],
    ) -> DomainResult<()> {
        let mut change = self.begin_change(conversation).await?;
        change.require_admin(admin)?;
        let mut added: Vec<UserId> = users
            .iter()
            .copied()
            .filter(|user| !change.is_current(*user))
            .collect();
        added.sort();
        added.dedup();
        if added.is_empty() {
            return change.cancel().await;
        }
        if change.current_count() + added.len() > MAX_GROUP_MEMBERS {
            return Err(DomainError::GroupFull);
        }
        for user in &added {
            if !are_contacts(&mut change.tx, admin, *user).await? {
                return Err(DomainError::NotContact);
            }
        }
        let event = SystemEvent::MembersAdded {
            actor: admin,
            users: added.clone(),
        };
        let seq = change.append(&event).await?;
        for user in added {
            join(&mut change.tx, conversation, user, Role::Member, seq).await?;
        }
        let events = change.finish(None).await?;
        self.publish(events).await;
        Ok(())
    }

    /// Removes a current member from a group. A `MemberRemoved` entry records it, and it is
    /// the last entry the removed user can see. Removing a former member again changes
    /// nothing; a user who was never a member is `NotMember`. Admins leave rather than
    /// remove themselves.
    pub async fn remove_member(
        &self,
        admin: UserId,
        conversation: ConversationId,
        user: UserId,
    ) -> DomainResult<()> {
        if admin == user {
            return Err(ValidationError::SelfTarget.into());
        }
        let mut change = self.begin_change(conversation).await?;
        change.require_admin(admin)?;
        let mut removed = change.member(user).cloned().ok_or(DomainError::NotMember)?;
        if !removed.is_current() {
            return change.cancel().await;
        }
        let event = SystemEvent::MemberRemoved { actor: admin, user };
        removed.left_at_seq = Some(change.append(&event).await?);
        change.put_member(removed).await?;
        let events = change.finish(Some(user)).await?;
        self.publish(events).await;
        Ok(())
    }

    /// `user` leaves a group. A `MemberLeft` entry records it, and it is the last entry they
    /// can see. Leaving again changes nothing.
    ///
    /// A group always has an admin: if none remains, the current member who joined first
    /// (lowest `visible_from_seq`, then lowest user id) becomes one, recorded by an
    /// `AdminPromoted` entry. When the last member leaves, the group is closed: nothing can
    /// be appended to it any more, because every append needs a current member.
    pub async fn leave_group(
        &self,
        user: UserId,
        conversation: ConversationId,
    ) -> DomainResult<()> {
        let mut change = self.begin_change(conversation).await?;
        let mut leaver = change.member(user).cloned().ok_or(DomainError::NotMember)?;
        if !leaver.is_current() {
            return change.cancel().await;
        }
        leaver.left_at_seq = Some(
            change
                .append(&SystemEvent::MemberLeft { actor: user })
                .await?,
        );
        change.put_member(leaver).await?;
        if let Some(mut heir) = change.heir() {
            let promoted = SystemEvent::AdminPromoted {
                actor: user,
                user: heir.user_id,
            };
            change.append(&promoted).await?;
            heir.role = Role::Admin;
            change.put_member(heir).await?;
        }
        let events = change.finish(Some(user)).await?;
        self.publish(events).await;
        Ok(())
    }

    /// Locks a group for a membership or settings change and takes the next `seq`.
    async fn begin_change(&self, conversation: ConversationId) -> DomainResult<GroupChange<S::Tx>> {
        let mut tx = self.store.begin().await?;
        let conversation = require_conversation(&mut tx, conversation).await?;
        if conversation.kind != ConversationKind::Group {
            return Err(DomainError::NotAGroup);
        }
        let now = self.clock.now();
        let seq = tx.next_seq(conversation.id, now).await?;
        let members = tx.members(conversation.id).await?;
        let before = group_positions(&mut tx, &conversation, seq - 1).await?;
        Ok(GroupChange {
            tx,
            conversation,
            members,
            next_seq: Some(seq),
            last_seq: seq - 1,
            now,
            before,
        })
    }
}

/// An open transaction holding a group's lock, for one membership or settings change.
/// Dropping it without `finish` rolls back, which also gives back the `seq` it took.
struct GroupChange<T> {
    tx: T,
    conversation: Conversation,
    /// Every member row as it was when the lock was taken, current and former.
    members: Vec<Member>,
    /// The `seq` taken by `begin_change` and not used yet.
    next_seq: Option<i64>,
    /// The highest `seq` written so far.
    last_seq: i64,
    now: DateTime<Utc>,
    /// The group-wide positions before the change.
    before: Option<GroupPositions>,
}

impl<T: Tx> GroupChange<T> {
    /// `NotMember` unless `actor` is a current member, `NotAdmin` unless also an admin.
    fn require_admin(&self, actor: UserId) -> DomainResult<()> {
        match self.member(actor) {
            Some(member) if member.is_current() && member.role == Role::Admin => Ok(()),
            Some(member) if member.is_current() => Err(DomainError::NotAdmin),
            _ => Err(DomainError::NotMember),
        }
    }

    fn is_current(&self, user: UserId) -> bool {
        self.member(user).is_some_and(Member::is_current)
    }

    fn member(&self, user: UserId) -> Option<&Member> {
        self.members.iter().find(|member| member.user_id == user)
    }

    fn current_count(&self) -> usize {
        self.members
            .iter()
            .filter(|member| member.is_current())
            .count()
    }

    /// The member to promote when no current admin is left: the current member who joined
    /// first, then the lowest user id. `None` while an admin remains or nobody does.
    fn heir(&self) -> Option<Member> {
        let current = self.members.iter().filter(|member| member.is_current());
        if current.clone().any(|member| member.role == Role::Admin) {
            return None;
        }
        current
            .min_by_key(|member| (member.visible_from_seq, member.user_id))
            .cloned()
    }

    /// Stores a changed member row and keeps this change's view of the members up to date.
    async fn put_member(&mut self, member: Member) -> DomainResult<()> {
        if let Some(row) = self
            .members
            .iter_mut()
            .find(|row| row.user_id == member.user_id)
        {
            *row = member.clone();
        }
        self.tx.insert_member(member).await
    }

    /// Appends a system entry recording `event` and returns its `seq`.
    async fn append(&mut self, event: &SystemEvent) -> DomainResult<i64> {
        let seq = match self.next_seq.take() {
            Some(seq) => seq,
            None => self.tx.next_seq(self.conversation.id, self.now).await?,
        };
        self.tx
            .insert_entry(system_entry(self.conversation.id, seq, event, self.now))
            .await?;
        self.last_seq = seq;
        Ok(seq)
    }

    /// Nothing to change: rolls back.
    async fn cancel(self) -> DomainResult<()> {
        self.tx.rollback().await
    }

    /// Commits, and returns the events to publish: new entries for the current members and
    /// `also_tell` (a member who just left and may see their own departure), and the new
    /// group-wide positions if they moved.
    async fn finish(mut self, also_tell: Option<UserId>) -> DomainResult<Vec<Event>> {
        let group =
            group_receipt(&mut self.tx, &self.conversation, self.last_seq, self.before).await?;
        let members = self.tx.members(self.conversation.id).await?;
        self.tx.commit().await?;

        let mut users = current_member_ids(&members);
        users.extend(also_tell);
        users.sort();
        let mut events = vec![Event::EntriesAvailable { users }];
        events.extend(group);
        Ok(events)
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

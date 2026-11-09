//! Groups of up to 100 members with admin and member roles (spec §6).

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::conversations::require_conversation;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ClientMsgId, ConversationId, MessageId, UserId};
use crate::model::{
    ContentType, Conversation, ConversationKind, Entry, EntryKind, GroupPositions, Member, Role,
};
use crate::notify::{Event, Notifier};
use crate::outgoing::{Ask, Gate, acting_agent, gate, still_waiting};
use crate::read::{group_positions_before, group_receipt};
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
    ///
    /// An agent's group passes its owner's outgoing policy first (spec 2026-11-09
    /// proactive agents §3), before the members are checked: `NotAllowedByOwner`, or
    /// `WaitingForOwner` with the waiting action. Its group is `started_by_agent`.
    pub async fn create_group(
        &self,
        creator: UserId,
        title: &str,
        members: &[UserId],
    ) -> DomainResult<ConversationId> {
        let (title, others) = group_parts(creator, title, members)?;
        let mut tx = self.store.begin().await?;
        if !tx.user_exists(creator).await? {
            return Err(DomainError::UnknownUser);
        }
        let now = self.clock.now();
        let by_agent = tx.agent_owner(creator).await?.is_some();
        if by_agent {
            if let Some(owner) = acting_agent(&mut tx, creator).await? {
                // Every user's lock in one call, before the gate takes the agent's again.
                // Nobody else can know the new group yet, so taking them before its
                // conversation lock cannot form a cycle.
                let mut users = others.clone();
                users.push(creator);
                tx.lock_users(&users).await?;
                let ask = Ask::group(title.clone(), others.clone());
                if let Gate::Waiting { action, new } = gate(&mut tx, creator, ask, now).await? {
                    return self.wait_for_owner(tx, owner, action, new).await;
                }
            }
        }
        let (id, events) = create_group_on(&mut tx, creator, title, &others, by_agent, now).await?;
        tx.commit().await?;
        self.publish(events).await;
        Ok(id)
    }

    /// Adds accepted contacts of `admin` to a group as members. Users who are already
    /// current members are skipped, so adding twice changes nothing; former members
    /// rejoin. A group has at most `MAX_GROUP_MEMBERS` current members.
    ///
    /// A `MembersAdded` entry records the change. The added members see the log from that
    /// entry on, with nothing before it unread, and every active device of theirs starts
    /// just before it.
    ///
    /// An agent adding members passes its owner's outgoing policy first (spec 2026-11-09
    /// proactive agents §3), after the admin check and before the new members are
    /// checked: `NotAllowedByOwner`, or `WaitingForOwner` with the waiting action.
    pub async fn add_members(
        &self,
        admin: UserId,
        conversation: ConversationId,
        users: &[UserId],
    ) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        let mut change = begin_change(&mut tx, conversation, self.clock.now()).await?;
        let Some(added) = change.to_add(admin, users)? else {
            return tx.rollback().await;
        };
        let mut locked = added.clone();
        if let Some(owner) = acting_agent(&mut *change.tx, admin).await? {
            // Every user's lock in one call, before the gate takes the agent's again.
            locked.push(admin);
            change.tx.lock_users(&locked).await?;
            let ask = Ask::invite(conversation, added.clone());
            if let Gate::Waiting { action, new } =
                gate(&mut *change.tx, admin, ask.clone(), change.now).await?
            {
                // The change took the group's next `seq`, so it cannot commit: the gate
                // runs again on a transaction of its own (and stores a new action there).
                // Should the policy have changed meanwhile, retry.
                tx.rollback().await?;
                if !new {
                    return Err(DomainError::WaitingForOwner {
                        action_id: action.action_id,
                    });
                }
                self.gate_alone(admin, owner, ask).await?;
                return Err(DomainError::Conflict);
            }
        }
        add_on(&mut change, admin, added, &locked).await?;
        let events = change.finish(None).await?;
        tx.commit().await?;
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
        let mut tx = self.store.begin().await?;
        let mut change = begin_change(&mut tx, conversation, self.clock.now()).await?;
        change.require_admin(admin)?;
        let mut removed = change.member(user).cloned().ok_or(DomainError::NotMember)?;
        if !removed.is_current() {
            return tx.rollback().await;
        }
        let event = SystemEvent::MemberRemoved { actor: admin, user };
        removed.left_at_seq = Some(change.append(&event).await?);
        change.put_member(removed).await?;
        let events = change.finish(Some(user)).await?;
        tx.commit().await?;
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
        let mut tx = self.store.begin().await?;
        let mut change = begin_change(&mut tx, conversation, self.clock.now()).await?;
        let mut leaver = change.member(user).cloned().ok_or(DomainError::NotMember)?;
        if !leaver.is_current() {
            return tx.rollback().await;
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
        tx.commit().await?;
        self.publish(events).await;
        Ok(())
    }

    /// Renames a group. The title is trimmed and must keep 1 to `MAX_TITLE_CHARS`
    /// characters. A `Renamed` entry records it; renaming to the current title changes
    /// nothing.
    pub async fn rename_group(
        &self,
        admin: UserId,
        conversation: ConversationId,
        title: &str,
    ) -> DomainResult<()> {
        let title = validate_title(title)?;
        let mut tx = self.store.begin().await?;
        let mut change = begin_change(&mut tx, conversation, self.clock.now()).await?;
        change.require_admin(admin)?;
        if change.conversation.title.as_deref() == Some(title.as_str()) {
            return tx.rollback().await;
        }
        let event = SystemEvent::Renamed {
            actor: admin,
            title: title.clone(),
        };
        change.append(&event).await?;
        change.tx.set_title(conversation, title).await?;
        let events = change.finish(None).await?;
        tx.commit().await?;
        self.publish(events).await;
        Ok(())
    }

    /// Gives a current member the role `role`. A `RoleChanged` entry records it; giving
    /// the role they already have changes nothing. Admins may demote themselves, but the
    /// last admin may not: a group always has an admin (`LastAdmin`).
    pub async fn set_role(
        &self,
        admin: UserId,
        conversation: ConversationId,
        user: UserId,
        role: Role,
    ) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        let mut change = begin_change(&mut tx, conversation, self.clock.now()).await?;
        change.require_admin(admin)?;
        let mut member = change
            .member(user)
            .filter(|member| member.is_current())
            .cloned()
            .ok_or(DomainError::NotMember)?;
        if member.role == role {
            return tx.rollback().await;
        }
        if role == Role::Member && change.admin_count() == 1 {
            return Err(DomainError::LastAdmin);
        }
        let event = SystemEvent::RoleChanged {
            actor: admin,
            user,
            role,
        };
        change.append(&event).await?;
        member.role = role;
        change.put_member(member).await?;
        let events = change.finish(None).await?;
        tx.commit().await?;
        self.publish(events).await;
        Ok(())
    }
}

/// The validated title of a new group of `creator` and its other members, sorted and
/// without duplicates or `creator`.
pub(crate) fn group_parts(
    creator: UserId,
    title: &str,
    members: &[UserId],
) -> DomainResult<(String, Vec<UserId>)> {
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
    Ok((title, others))
}

/// Creates the group of [`Messaging::create_group`] on `tx`, past the creator's gate:
/// `NotContact` unless every one of `others` is an accepted contact of `creator`. Returns
/// the group and the events to publish once `tx` commits.
pub(crate) async fn create_group_on<T: Tx>(
    tx: &mut T,
    creator: UserId,
    title: String,
    others: &[UserId],
    by_agent: bool,
    now: DateTime<Utc>,
) -> DomainResult<(ConversationId, Vec<Event>)> {
    for other in others {
        if !are_contacts(tx, creator, *other).await? {
            return Err(DomainError::NotContact);
        }
    }
    let conversation = Conversation {
        id: ConversationId::new(),
        kind: ConversationKind::Group,
        direct_key: None,
        title: Some(title.clone()),
        created_by: creator,
        last_seq: 0,
        created_at: now,
        updated_at: now,
        started_by_agent: by_agent,
    };
    let id = conversation.id;
    // A group has no direct key, so the insert cannot be a duplicate.
    tx.insert_conversation(conversation).await?;
    let seq = tx.next_seq(id, now).await?;
    let created = SystemEvent::Created {
        actor: creator,
        title,
        users: others.to_vec(),
    };
    tx.insert_entry(system_entry(id, seq, &created, now))
        .await?;
    let mut users = others.to_vec();
    users.push(creator);
    // Taken already when the gate ran; taking them again changes nothing.
    tx.lock_users(&users).await?;
    join(tx, id, creator, Role::Admin, seq).await?;
    for other in others {
        join(tx, id, *other, Role::Member, seq).await?;
    }
    users.sort();
    Ok((id, vec![Event::EntriesAvailable { users }]))
}

/// Carries out `agent`'s approved invite `action_id` of `members` to `conversation` on
/// `tx`: the group's lock, then the users' (the agent's included), then
/// [`still_waiting`], then the change. Returns the events to publish once `tx` commits;
/// `None` when nothing changes (every one of `members` is a current member), and then the
/// caller must undo the `seq` the change took.
pub(crate) async fn approved_invite<T: Tx>(
    tx: &mut T,
    agent: UserId,
    action_id: Uuid,
    conversation: ConversationId,
    members: &[UserId],
    now: DateTime<Utc>,
) -> DomainResult<Option<Vec<Event>>> {
    let mut change = begin_change(tx, conversation, now).await?;
    let added = change.to_add(agent, members)?;
    let mut locked = added.clone().unwrap_or_default();
    locked.push(agent);
    change.tx.lock_users(&locked).await?;
    still_waiting(&mut *change.tx, agent, action_id).await?;
    let Some(added) = added else {
        return Ok(None);
    };
    add_on(&mut change, agent, added, &locked).await?;
    Ok(Some(change.finish(None).await?))
}

/// Adds `added` (from [`GroupChange::to_add`]) to the group: `NotContact` unless each is
/// an accepted contact of `admin`. Takes the locks of `locked` (`added`, and the agent
/// when the gate ran) in one call.
async fn add_on<T: Tx>(
    change: &mut GroupChange<'_, T>,
    admin: UserId,
    added: Vec<UserId>,
    locked: &[UserId],
) -> DomainResult<()> {
    for user in &added {
        if !are_contacts(&mut *change.tx, admin, *user).await? {
            return Err(DomainError::NotContact);
        }
    }
    let event = SystemEvent::MembersAdded {
        actor: admin,
        users: added.clone(),
    };
    // Taken already when the gate ran; taking them again changes nothing.
    change.tx.lock_users(locked).await?;
    let seq = change.append(&event).await?;
    for user in added {
        join(
            &mut *change.tx,
            change.conversation.id,
            user,
            Role::Member,
            seq,
        )
        .await?;
    }
    Ok(())
}

/// Locks a group for a membership or settings change and takes the next `seq`. Rolling
/// `tx` back without committing gives back the `seq`.
async fn begin_change<T: Tx>(
    tx: &mut T,
    conversation: ConversationId,
    now: DateTime<Utc>,
) -> DomainResult<GroupChange<'_, T>> {
    let seq = tx.next_seq(conversation, now).await?;
    // Read under the lock, so the title compared by a rename is the current one.
    let conversation = require_conversation(tx, conversation).await?;
    if conversation.kind != ConversationKind::Group {
        return Err(DomainError::NotAGroup);
    }
    let members = tx.members(conversation.id).await?;
    let before = group_positions_before(tx, &conversation, seq - 1).await?;
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

/// A transaction holding a group's lock, for one membership or settings change. Its
/// caller commits after [`GroupChange::finish`], or rolls back when nothing changes.
struct GroupChange<'t, T> {
    tx: &'t mut T,
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

impl<T: Tx> GroupChange<'_, T> {
    /// `NotMember` unless `actor` is a current member, `NotAdmin` unless also an admin.
    fn require_admin(&self, actor: UserId) -> DomainResult<()> {
        match self.member(actor) {
            Some(member) if member.is_current() && member.role == Role::Admin => Ok(()),
            Some(member) if member.is_current() => Err(DomainError::NotAdmin),
            _ => Err(DomainError::NotMember),
        }
    }

    /// The users of `users` that `admin` would add (sorted, without duplicates or current
    /// members), after the admin check; `None` when there are none. `GroupFull` when they
    /// do not fit.
    fn to_add(&self, admin: UserId, users: &[UserId]) -> DomainResult<Option<Vec<UserId>>> {
        self.require_admin(admin)?;
        let mut added: Vec<UserId> = users
            .iter()
            .copied()
            .filter(|user| !self.is_current(*user))
            .collect();
        added.sort();
        added.dedup();
        if added.is_empty() {
            return Ok(None);
        }
        if self.current_count() + added.len() > MAX_GROUP_MEMBERS {
            return Err(DomainError::GroupFull);
        }
        Ok(Some(added))
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

    fn admin_count(&self) -> usize {
        self.members
            .iter()
            .filter(|member| member.is_current() && member.role == Role::Admin)
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

    /// The events to publish once the change commits: new entries for the current members
    /// and `also_tell` (a member who just left and may see their own departure), and the
    /// new group-wide positions if they moved.
    async fn finish(self, also_tell: Option<UserId>) -> DomainResult<Vec<Event>> {
        let group = group_receipt(
            &mut *self.tx,
            &self.conversation,
            self.last_seq,
            self.before,
        )
        .await?;
        let members = self.tx.members(self.conversation.id).await?;

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
/// member rejoining) keeps it; readers never look below `visible_from_seq - 1`. The
/// caller holds `user`'s lock ([`Tx::lock_users`]), so no device of theirs is being
/// registered meanwhile.
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
        answer_seq: None,
    }
}

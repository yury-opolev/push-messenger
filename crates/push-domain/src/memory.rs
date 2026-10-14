//! An in-memory [`Store`] for tests.
//!
//! Each transaction holds a store-wide lock and works on a copy of the state, so
//! transactions are fully serialised and rollback is free. Never begin a second
//! transaction while one is open in the same task: it would deadlock.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::error::{DomainError, DomainResult};
use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
use crate::model::{
    ContactState, Conversation, ConversationKind, ConversationOverview, Entry, EntryKind,
    GroupPositions, InviteRecord, Member, Role,
};
use crate::store::{InsertOutcome, Store, Tx};

fn missing(what: &str) -> DomainError {
    DomainError::Store(format!("foreign key violation: {what} does not exist"))
}

#[derive(Debug, Clone, Default)]
struct State {
    users: HashSet<UserId>,
    devices: HashMap<DeviceId, DeviceRow>,
    contacts: HashMap<(UserId, UserId), ContactState>,
    invites: HashMap<String, InviteRecord>,
    conversations: HashMap<ConversationId, Conversation>,
    direct_index: HashMap<[u8; 32], ConversationId>,
    members: BTreeMap<(ConversationId, UserId), Member>,
    entries: BTreeMap<(ConversationId, i64), Entry>,
    client_ids: HashMap<(ConversationId, ClientMsgId), i64>,
    cursors: HashMap<(DeviceId, ConversationId), i64>,
    message_ids: HashMap<MessageId, (ConversationId, i64)>,
}

impl State {
    fn first_live_message(
        &self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        not_from: Option<UserId>,
    ) -> Option<i64> {
        if after >= up_to {
            return None;
        }
        self.entries
            .range((conversation, after + 1)..=(conversation, up_to))
            .map(|(_, entry)| entry)
            .find(|entry| {
                entry.kind == EntryKind::Message
                    && entry.deleted_at.is_none()
                    && not_from.is_none_or(|user| entry.sender_id != user)
            })
            .map(|entry| entry.seq)
    }

    fn delivered_seq(&self, conversation: ConversationId, user: UserId) -> i64 {
        self.cursors
            .iter()
            .filter(|((device, cursor_conversation), _)| {
                *cursor_conversation == conversation
                    && self.devices.get(device).is_some_and(|row| row.user == user)
            })
            .map(|(_, position)| *position)
            .max()
            .unwrap_or(0)
    }

    fn count_incoming_messages(
        &self,
        conversation: ConversationId,
        user: UserId,
        after: i64,
        up_to: i64,
        cap: u32,
    ) -> u32 {
        if after >= up_to {
            return 0;
        }
        self.entries
            .range((conversation, after + 1)..=(conversation, up_to))
            .map(|(_, entry)| entry)
            .filter(|entry| {
                entry.kind == EntryKind::Message
                    && entry.deleted_at.is_none()
                    && entry.sender_id != user
            })
            .take(cap as usize)
            .count() as u32
    }

    fn group_positions(
        &self,
        conversation: ConversationId,
        last_seq: i64,
    ) -> Option<GroupPositions> {
        self.members
            .values()
            .filter(|member| member.conversation_id == conversation && member.is_current())
            .map(|member| {
                let delivered_seq = self.delivered_seq(conversation, member.user_id);
                GroupPositions {
                    delivered: self.effective_position(
                        member,
                        last_seq,
                        delivered_seq.max(member.read_seq),
                    ),
                    read: self.effective_position(member, last_seq, member.read_seq),
                }
            })
            .reduce(|all, one| GroupPositions {
                delivered: all.delivered.min(one.delivered),
                read: all.read.min(one.read),
            })
    }

    /// See [`Tx::conversation_overviews`].
    fn overview(
        &self,
        conversation: &Conversation,
        member: &Member,
        unread_cap: u32,
    ) -> ConversationOverview {
        let id = conversation.id;
        let rows: Vec<&Member> = self
            .members
            .values()
            .filter(|row| row.conversation_id == id)
            .collect();
        // Sorted by user id (members is keyed by (ConversationId, UserId)); the contract requires it.
        let current = |admins_only: bool| -> Vec<UserId> {
            rows.iter()
                .filter(|row| row.is_current() && (!admins_only || row.role == Role::Admin))
                .map(|row| row.user_id)
                .collect()
        };
        let peer = (conversation.kind == ConversationKind::Direct)
            .then(|| rows.iter().find(|row| row.user_id != member.user_id))
            .flatten()
            .map(|peer| ((*peer).clone(), self.delivered_seq(id, peer.user_id)));
        let left_at = member
            .left_at_seq
            .and_then(|seq| self.entries.get(&(id, seq)).map(|entry| entry.server_ts));
        ConversationOverview {
            conversation: conversation.clone(),
            member: member.clone(),
            members: current(false),
            admins: current(true),
            peer,
            unread: self.count_incoming_messages(
                id,
                member.user_id,
                member.read_seq.max(member.visible_from_seq - 1),
                member.visible_upper(conversation.last_seq),
                unread_cap,
            ),
            group: (conversation.kind == ConversationKind::Group)
                .then(|| self.group_positions(id, conversation.last_seq))
                .flatten(),
            left_at,
        }
    }

    /// See [`Tx::group_positions`].
    fn effective_position(&self, member: &Member, last_seq: i64, position: i64) -> i64 {
        let upper = member.visible_upper(last_seq);
        let from = position.max(member.visible_from_seq - 1);
        if from >= upper {
            return upper;
        }
        self.first_live_message(member.conversation_id, from, upper, Some(member.user_id))
            .map_or(upper, |seq| seq - 1)
    }
}

#[derive(Debug, Clone, Copy)]
struct DeviceRow {
    user: UserId,
    revoked: bool,
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryStore {
    state: Arc<Mutex<State>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug)]
pub struct InMemoryTx {
    guard: OwnedMutexGuard<State>,
    work: State,
}

#[async_trait]
impl Store for InMemoryStore {
    type Tx = InMemoryTx;

    async fn begin(&self) -> DomainResult<InMemoryTx> {
        let guard = Arc::clone(&self.state).lock_owned().await;
        let work = guard.clone();
        Ok(InMemoryTx { guard, work })
    }
}

#[async_trait]
impl Tx for InMemoryTx {
    async fn commit(self) -> DomainResult<()> {
        let InMemoryTx { mut guard, work } = self;
        *guard = work;
        Ok(())
    }

    async fn rollback(self) -> DomainResult<()> {
        Ok(())
    }

    async fn insert_user(&mut self, user: UserId) -> DomainResult<()> {
        self.work.users.insert(user);
        Ok(())
    }

    async fn user_exists(&mut self, user: UserId) -> DomainResult<bool> {
        Ok(self.work.users.contains(&user))
    }

    async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<bool> {
        if let Some(row) = self.work.devices.get(&device) {
            return Ok(row.user == user && !row.revoked);
        }
        if !self.work.users.contains(&user) {
            return Err(missing("user"));
        }
        self.work.devices.insert(
            device,
            DeviceRow {
                user,
                revoked: false,
            },
        );
        Ok(true)
    }

    async fn revoke_device(&mut self, device: DeviceId) -> DomainResult<()> {
        if let Some(row) = self.work.devices.get_mut(&device) {
            row.revoked = true;
        }
        Ok(())
    }

    async fn device_user(&mut self, device: DeviceId) -> DomainResult<Option<UserId>> {
        Ok(self.work.devices.get(&device).map(|row| row.user))
    }

    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>> {
        Ok(self
            .work
            .devices
            .get(&device)
            .filter(|row| !row.revoked)
            .map(|row| row.user))
    }

    async fn devices_of_user(&mut self, user: UserId) -> DomainResult<Vec<DeviceId>> {
        let mut devices: Vec<DeviceId> = self
            .work
            .devices
            .iter()
            .filter(|(_, row)| row.user == user && !row.revoked)
            .map(|(id, _)| *id)
            .collect();
        devices.sort();
        Ok(devices)
    }

    async fn lock_contact_pair(&mut self, _a: UserId, _b: UserId) -> DomainResult<()> {
        // Every in-memory transaction already holds the store-wide lock.
        Ok(())
    }

    async fn lock_users(&mut self, _users: &[UserId]) -> DomainResult<()> {
        // Every in-memory transaction already holds the store-wide lock.
        Ok(())
    }

    async fn get_contact(
        &mut self,
        owner: UserId,
        peer: UserId,
    ) -> DomainResult<Option<ContactState>> {
        Ok(self.work.contacts.get(&(owner, peer)).copied())
    }

    async fn put_contact(
        &mut self,
        owner: UserId,
        peer: UserId,
        state: ContactState,
    ) -> DomainResult<()> {
        if !self.work.users.contains(&owner) || !self.work.users.contains(&peer) {
            return Err(missing("user"));
        }
        self.work.contacts.insert((owner, peer), state);
        Ok(())
    }

    async fn delete_contact(&mut self, owner: UserId, peer: UserId) -> DomainResult<()> {
        self.work.contacts.remove(&(owner, peer));
        Ok(())
    }

    async fn contacts_of(&mut self, owner: UserId) -> DomainResult<Vec<(UserId, ContactState)>> {
        Ok(self
            .work
            .contacts
            .iter()
            .filter(|((row_owner, _), _)| *row_owner == owner)
            .map(|((_, peer), state)| (*peer, *state))
            .collect())
    }

    async fn insert_invite(&mut self, invite: InviteRecord) -> DomainResult<InsertOutcome> {
        if !self.work.users.contains(&invite.user_id) {
            return Err(missing("user"));
        }
        if self.work.invites.contains_key(&invite.code) {
            return Ok(InsertOutcome::Duplicate);
        }
        self.work.invites.insert(invite.code.clone(), invite);
        Ok(InsertOutcome::Inserted)
    }

    async fn lock_invite(&mut self, code: &str) -> DomainResult<Option<InviteRecord>> {
        // Every in-memory transaction already holds the store-wide lock.
        Ok(self.work.invites.get(code).cloned())
    }

    async fn update_invite(&mut self, invite: InviteRecord) -> DomainResult<()> {
        let stored =
            self.work.invites.get_mut(&invite.code).ok_or_else(|| {
                DomainError::Store("update of an invite that does not exist".into())
            })?;
        stored.uses = invite.uses;
        stored.revoked_at = invite.revoked_at;
        Ok(())
    }

    async fn find_direct(&mut self, key: [u8; 32]) -> DomainResult<Option<ConversationId>> {
        Ok(self.work.direct_index.get(&key).copied())
    }

    async fn insert_conversation(
        &mut self,
        conversation: Conversation,
    ) -> DomainResult<InsertOutcome> {
        if self.work.conversations.contains_key(&conversation.id) {
            return Err(DomainError::Store(
                "primary key violation: conversation already exists".into(),
            ));
        }
        if let Some(key) = conversation.direct_key {
            if self.work.direct_index.contains_key(&key) {
                return Ok(InsertOutcome::Duplicate);
            }
            self.work.direct_index.insert(key, conversation.id);
        }
        self.work
            .conversations
            .insert(conversation.id, conversation);
        Ok(InsertOutcome::Inserted)
    }

    async fn get_conversation(&mut self, id: ConversationId) -> DomainResult<Option<Conversation>> {
        Ok(self.work.conversations.get(&id).cloned())
    }

    async fn next_seq(&mut self, id: ConversationId, at: DateTime<Utc>) -> DomainResult<i64> {
        let conversation = self
            .work
            .conversations
            .get_mut(&id)
            .ok_or(DomainError::ConversationNotFound)?;
        conversation.last_seq += 1;
        conversation.updated_at = conversation.updated_at.max(at);
        Ok(conversation.last_seq)
    }

    async fn set_title(&mut self, id: ConversationId, title: String) -> DomainResult<()> {
        let conversation = self
            .work
            .conversations
            .get_mut(&id)
            .ok_or(DomainError::ConversationNotFound)?;
        conversation.title = Some(title);
        Ok(())
    }

    async fn insert_member(&mut self, member: Member) -> DomainResult<()> {
        if !self
            .work
            .conversations
            .contains_key(&member.conversation_id)
        {
            return Err(missing("conversation"));
        }
        if !self.work.users.contains(&member.user_id) {
            return Err(missing("user"));
        }
        self.work
            .members
            .insert((member.conversation_id, member.user_id), member);
        Ok(())
    }

    async fn get_member(
        &mut self,
        conversation: ConversationId,
        user: UserId,
    ) -> DomainResult<Option<Member>> {
        Ok(self.work.members.get(&(conversation, user)).cloned())
    }

    async fn members(&mut self, conversation: ConversationId) -> DomainResult<Vec<Member>> {
        Ok(self
            .work
            .members
            .values()
            .filter(|member| member.conversation_id == conversation)
            .cloned()
            .collect())
    }

    async fn memberships(&mut self, user: UserId) -> DomainResult<Vec<(Conversation, Member)>> {
        Ok(self
            .work
            .members
            .values()
            .filter(|member| member.user_id == user)
            .filter_map(|member| {
                self.work
                    .conversations
                    .get(&member.conversation_id)
                    .map(|conversation| (conversation.clone(), member.clone()))
            })
            .collect())
    }

    async fn raise_read_seq(
        &mut self,
        conversation: ConversationId,
        user: UserId,
        seq: i64,
    ) -> DomainResult<bool> {
        let member = self
            .work
            .members
            .get_mut(&(conversation, user))
            .ok_or(DomainError::NotMember)?;
        if seq > member.read_seq {
            member.read_seq = seq;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn lock_conversation_positions(
        &mut self,
        _conversation: ConversationId,
    ) -> DomainResult<()> {
        // Every in-memory transaction already holds the store-wide lock.
        Ok(())
    }

    async fn insert_entry(&mut self, entry: Entry) -> DomainResult<InsertOutcome> {
        if !self.work.conversations.contains_key(&entry.conversation_id) {
            return Err(missing("conversation"));
        }
        let key = (entry.conversation_id, entry.client_msg_id);
        if self.work.client_ids.contains_key(&key) {
            return Ok(InsertOutcome::Duplicate);
        }
        if self
            .work
            .entries
            .contains_key(&(entry.conversation_id, entry.seq))
        {
            return Err(DomainError::Store(
                "primary key violation: an entry with this seq already exists".into(),
            ));
        }
        if self.work.message_ids.contains_key(&entry.message_id) {
            return Err(DomainError::Store(
                "unique violation: message_id already exists".into(),
            ));
        }
        self.work
            .message_ids
            .insert(entry.message_id, (entry.conversation_id, entry.seq));
        self.work.client_ids.insert(key, entry.seq);
        self.work
            .entries
            .insert((entry.conversation_id, entry.seq), entry);
        Ok(InsertOutcome::Inserted)
    }

    async fn entry_by_client_id(
        &mut self,
        conversation: ConversationId,
        client_msg_id: ClientMsgId,
    ) -> DomainResult<Option<Entry>> {
        Ok(self
            .work
            .client_ids
            .get(&(conversation, client_msg_id))
            .and_then(|seq| self.work.entries.get(&(conversation, *seq)))
            .cloned())
    }

    async fn entry_by_message_id(
        &mut self,
        conversation: ConversationId,
        message_id: MessageId,
    ) -> DomainResult<Option<Entry>> {
        Ok(self
            .work
            .message_ids
            .get(&message_id)
            .filter(|(owner, _)| *owner == conversation)
            .and_then(|key| self.work.entries.get(key))
            .cloned())
    }

    async fn update_entry(&mut self, entry: Entry) -> DomainResult<()> {
        let stored = self
            .work
            .entries
            .get_mut(&(entry.conversation_id, entry.seq))
            .ok_or_else(|| DomainError::Store("update of an entry that does not exist".into()))?;
        stored.body = entry.body;
        stored.edited_at = entry.edited_at;
        stored.edit_seq = entry.edit_seq;
        stored.deleted_at = entry.deleted_at;
        Ok(())
    }

    async fn erase_edit_bodies(
        &mut self,
        conversation: ConversationId,
        target_seq: i64,
    ) -> DomainResult<()> {
        for entry in self
            .work
            .entries
            .range_mut((conversation, i64::MIN)..=(conversation, i64::MAX))
            .map(|(_, entry)| entry)
            .filter(|entry| entry.kind == EntryKind::Edit && entry.target_seq == Some(target_seq))
        {
            entry.body = None;
        }
        Ok(())
    }

    async fn entries_after(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        targets_from: i64,
        limit: u32,
    ) -> DomainResult<Vec<Entry>> {
        if after >= up_to {
            return Ok(Vec::new());
        }
        Ok(self
            .work
            .entries
            .range((conversation, after + 1)..=(conversation, up_to))
            .map(|(_, entry)| entry)
            .filter(|entry| entry.target_seq.is_none_or(|target| target >= targets_from))
            .take(limit as usize)
            .cloned()
            .collect())
    }

    async fn count_incoming_messages(
        &mut self,
        conversation: ConversationId,
        user: UserId,
        after: i64,
        up_to: i64,
        cap: u32,
    ) -> DomainResult<u32> {
        Ok(self
            .work
            .count_incoming_messages(conversation, user, after, up_to, cap))
    }

    async fn first_live_message(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        not_from: Option<UserId>,
    ) -> DomainResult<Option<i64>> {
        Ok(self
            .work
            .first_live_message(conversation, after, up_to, not_from))
    }

    async fn messages_before(
        &mut self,
        conversation: ConversationId,
        from: i64,
        before: i64,
        limit: u32,
    ) -> DomainResult<Vec<Entry>> {
        if from >= before {
            return Ok(Vec::new());
        }
        let mut page: Vec<Entry> = self
            .work
            .entries
            .range((conversation, from)..(conversation, before))
            .rev()
            .map(|(_, entry)| entry)
            .filter(|entry| matches!(entry.kind, EntryKind::Message | EntryKind::System))
            .take(limit as usize)
            .cloned()
            .collect();
        page.reverse();
        Ok(page)
    }

    async fn get_cursor(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
    ) -> DomainResult<Option<i64>> {
        Ok(self.work.cursors.get(&(device, conversation)).copied())
    }

    async fn create_cursor_if_absent(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
        seq: i64,
    ) -> DomainResult<()> {
        if !self.work.devices.contains_key(&device) {
            return Err(missing("device"));
        }
        if !self.work.conversations.contains_key(&conversation) {
            return Err(missing("conversation"));
        }
        self.work
            .cursors
            .entry((device, conversation))
            .or_insert(seq);
        Ok(())
    }

    async fn raise_cursor(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
        seq: i64,
    ) -> DomainResult<bool> {
        if !self.work.devices.contains_key(&device) {
            return Err(missing("device"));
        }
        if !self.work.conversations.contains_key(&conversation) {
            return Err(missing("conversation"));
        }
        match self.work.cursors.get_mut(&(device, conversation)) {
            Some(position) if seq > *position => {
                *position = seq;
                Ok(true)
            }
            Some(_) => Ok(false),
            None => {
                self.work.cursors.insert((device, conversation), seq);
                Ok(true)
            }
        }
    }

    async fn delivered_seq(
        &mut self,
        conversation: ConversationId,
        user: UserId,
    ) -> DomainResult<i64> {
        Ok(self.work.delivered_seq(conversation, user))
    }

    async fn member_positions(
        &mut self,
        conversation: ConversationId,
    ) -> DomainResult<Vec<(Member, i64)>> {
        let state = &self.work;
        Ok(state
            .members
            .values()
            .filter(|member| member.conversation_id == conversation)
            .map(|member| {
                let delivered_seq = state.delivered_seq(conversation, member.user_id);
                (member.clone(), delivered_seq)
            })
            .collect())
    }

    async fn sync_positions(
        &mut self,
        user: UserId,
        device: DeviceId,
    ) -> DomainResult<Vec<(Conversation, Member, i64)>> {
        let state = &self.work;
        Ok(state
            .members
            .values()
            .filter(|member| member.user_id == user)
            .filter_map(|member| {
                let conversation = state.conversations.get(&member.conversation_id)?;
                let floor = member.visible_from_seq - 1;
                let position = state
                    .cursors
                    .get(&(device, conversation.id))
                    .map_or(floor, |position| (*position).max(floor));
                (position < member.visible_upper(conversation.last_seq))
                    .then(|| (conversation.clone(), member.clone(), position))
            })
            .collect())
    }

    async fn group_positions(
        &mut self,
        conversation: ConversationId,
        last_seq: i64,
    ) -> DomainResult<Option<GroupPositions>> {
        Ok(self.work.group_positions(conversation, last_seq))
    }

    async fn conversation_overviews(
        &mut self,
        user: UserId,
        changed_since: Option<DateTime<Utc>>,
        unread_cap: u32,
    ) -> DomainResult<Vec<ConversationOverview>> {
        let state = &self.work;
        Ok(state
            .members
            .values()
            .filter(|member| member.user_id == user)
            .filter_map(|member| {
                let conversation = state.conversations.get(&member.conversation_id)?;
                if changed_since.is_some_and(|since| conversation.updated_at <= since) {
                    return None;
                }
                Some(state.overview(conversation, member, unread_cap))
            })
            .collect())
    }
}

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
use crate::model::{ContactState, Conversation, Entry, EntryKind, InviteRecord, Member};
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

    async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<()> {
        if !self.work.users.contains(&user) {
            return Err(missing("user"));
        }
        self.work.devices.entry(device).or_insert(DeviceRow {
            user,
            revoked: false,
        });
        Ok(())
    }

    async fn revoke_device(&mut self, device: DeviceId) -> DomainResult<()> {
        if let Some(row) = self.work.devices.get_mut(&device) {
            row.revoked = true;
        }
        Ok(())
    }

    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>> {
        Ok(self
            .work
            .devices
            .get(&device)
            .filter(|row| !row.revoked)
            .map(|row| row.user))
    }

    async fn device_exists(&mut self, device: DeviceId) -> DomainResult<bool> {
        Ok(self.work.devices.contains_key(&device))
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
        conversation.updated_at = at;
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
        if after >= up_to {
            return Ok(0);
        }
        let count = self
            .work
            .entries
            .range((conversation, after + 1)..=(conversation, up_to))
            .map(|(_, entry)| entry)
            .filter(|entry| {
                entry.kind == EntryKind::Message
                    && entry.deleted_at.is_none()
                    && entry.sender_id != user
            })
            .take(cap as usize)
            .count();
        Ok(count as u32)
    }

    async fn first_live_message(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        not_from: Option<UserId>,
    ) -> DomainResult<Option<i64>> {
        if after >= up_to {
            return Ok(None);
        }
        Ok(self
            .work
            .entries
            .range((conversation, after + 1)..=(conversation, up_to))
            .map(|(_, entry)| entry)
            .find(|entry| {
                entry.kind == EntryKind::Message
                    && entry.deleted_at.is_none()
                    && not_from.is_none_or(|user| entry.sender_id != user)
            })
            .map(|entry| entry.seq))
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
        Ok(self
            .work
            .cursors
            .iter()
            .filter(|((device, cursor_conversation), _)| {
                *cursor_conversation == conversation
                    && self
                        .work
                        .devices
                        .get(device)
                        .is_some_and(|row| row.user == user)
            })
            .map(|(_, position)| *position)
            .max()
            .unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ContentType, ConversationKind, Role};
    use chrono::{TimeDelta, TimeZone};

    fn start() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    fn conversation() -> Conversation {
        Conversation {
            id: ConversationId::new(),
            kind: ConversationKind::Direct,
            direct_key: None,
            title: None,
            created_by: UserId::new(),
            last_seq: 0,
            created_at: start(),
            updated_at: start(),
        }
    }

    fn entry(conversation: ConversationId, seq: i64, client_msg_id: ClientMsgId) -> Entry {
        Entry {
            conversation_id: conversation,
            seq,
            message_id: MessageId::new(),
            client_msg_id,
            kind: EntryKind::Message,
            target_seq: None,
            sender_id: UserId::new(),
            sender_device_id: None,
            content_type: ContentType::Text,
            body: Some(b"hi".to_vec()),
            client_ts: None,
            server_ts: start(),
            edited_at: None,
            edit_seq: None,
            deleted_at: None,
        }
    }

    fn seqs(entries: &[Entry]) -> Vec<i64> {
        entries.iter().map(|entry| entry.seq).collect()
    }

    fn invite(code: &str, user: UserId) -> InviteRecord {
        InviteRecord {
            code: code.into(),
            user_id: user,
            expires_at: start() + TimeDelta::days(1),
            max_uses: 1,
            uses: 0,
            revoked_at: None,
        }
    }

    #[tokio::test]
    async fn an_invite_code_is_stored_once() {
        let store = InMemoryStore::new();
        let (first, second) = (UserId::new(), UserId::new());
        let mut tx = store.begin().await.unwrap();
        tx.insert_user(first).await.unwrap();
        tx.insert_user(second).await.unwrap();

        assert_eq!(
            tx.insert_invite(invite("ABCDEFGHJK", first)).await,
            Ok(InsertOutcome::Inserted)
        );
        assert_eq!(
            tx.insert_invite(invite("ABCDEFGHJK", second)).await,
            Ok(InsertOutcome::Duplicate)
        );
        assert_eq!(
            tx.lock_invite("ABCDEFGHJK").await,
            Ok(Some(invite("ABCDEFGHJK", first)))
        );
        assert_eq!(tx.lock_invite("0000000000").await, Ok(None));
        assert!(matches!(
            tx.insert_invite(invite("0000000000", UserId::new())).await,
            Err(DomainError::Store(_))
        ));
    }

    #[tokio::test]
    async fn update_invite_changes_only_uses_and_revocation() {
        let store = InMemoryStore::new();
        let user = UserId::new();
        let mut tx = store.begin().await.unwrap();
        tx.insert_user(user).await.unwrap();
        tx.insert_invite(invite("ABCDEFGHJK", user)).await.unwrap();

        let mut changed = invite("ABCDEFGHJK", UserId::new());
        changed.uses = 1;
        changed.revoked_at = Some(start());
        changed.max_uses = 9;
        tx.update_invite(changed).await.unwrap();

        let stored = tx.lock_invite("ABCDEFGHJK").await.unwrap().unwrap();
        assert_eq!((stored.uses, stored.revoked_at), (1, Some(start())));
        assert_eq!((stored.user_id, stored.max_uses), (user, 1));
        assert!(matches!(
            tx.update_invite(invite("0000000000", user)).await,
            Err(DomainError::Store(_))
        ));
    }

    #[tokio::test]
    async fn committed_changes_are_visible_to_later_transactions() {
        let store = InMemoryStore::new();
        let user = UserId::new();
        let mut tx = store.begin().await.unwrap();
        tx.insert_user(user).await.unwrap();
        tx.commit().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        assert!(tx.user_exists(user).await.unwrap());
    }

    #[tokio::test]
    async fn rolled_back_and_dropped_transactions_leave_no_trace() {
        let store = InMemoryStore::new();
        let (rolled_back, dropped) = (UserId::new(), UserId::new());

        let mut tx = store.begin().await.unwrap();
        tx.insert_user(rolled_back).await.unwrap();
        tx.rollback().await.unwrap();
        {
            let mut tx = store.begin().await.unwrap();
            tx.insert_user(dropped).await.unwrap();
        }

        let mut tx = store.begin().await.unwrap();
        assert!(!tx.user_exists(rolled_back).await.unwrap());
        assert!(!tx.user_exists(dropped).await.unwrap());
    }

    #[tokio::test]
    async fn next_seq_counts_from_one_and_a_rollback_gives_the_number_back() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;

        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        assert_eq!(tx.next_seq(id, start()).await.unwrap(), 1);
        tx.commit().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        assert_eq!(tx.next_seq(id, start()).await.unwrap(), 2);
        tx.rollback().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        assert_eq!(tx.next_seq(id, start()).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn next_seq_moves_updated_at_and_a_rollback_restores_it() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let (first, second) = (start() + TimeDelta::hours(1), start() + TimeDelta::hours(2));

        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        tx.next_seq(id, first).await.unwrap();
        tx.commit().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        tx.next_seq(id, second).await.unwrap();
        tx.rollback().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        let stored = tx.get_conversation(id).await.unwrap().unwrap();
        assert_eq!(stored.updated_at, first);
    }

    #[tokio::test]
    async fn a_second_direct_conversation_for_the_same_pair_is_a_duplicate() {
        let store = InMemoryStore::new();
        let mut first = conversation();
        first.direct_key = Some([7; 32]);
        let mut second = conversation();
        second.direct_key = Some([7; 32]);
        let second_id = second.id;

        let mut tx = store.begin().await.unwrap();
        assert_eq!(
            tx.insert_conversation(first.clone()).await.unwrap(),
            InsertOutcome::Inserted
        );
        assert_eq!(
            tx.insert_conversation(second).await.unwrap(),
            InsertOutcome::Duplicate
        );
        assert_eq!(tx.find_direct([7; 32]).await.unwrap(), Some(first.id));
        assert_eq!(tx.get_conversation(second_id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_reused_client_msg_id_is_a_duplicate() {
        let store = InMemoryStore::new();
        let client_msg_id = ClientMsgId::new();
        let conversation = conversation();
        let id = conversation.id;

        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        let conversation = id;
        assert_eq!(
            tx.insert_entry(entry(conversation, 1, client_msg_id))
                .await
                .unwrap(),
            InsertOutcome::Inserted
        );
        assert_eq!(
            tx.insert_entry(entry(conversation, 2, client_msg_id))
                .await
                .unwrap(),
            InsertOutcome::Duplicate
        );
        let original = tx
            .entry_by_client_id(conversation, client_msg_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.seq, 1);
    }

    #[tokio::test]
    async fn entry_ranges_respect_their_bounds() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        let conversation = id;
        for seq in 1..=5 {
            tx.insert_entry(entry(conversation, seq, ClientMsgId::new()))
                .await
                .unwrap();
        }

        assert_eq!(
            seqs(&tx.entries_after(conversation, 1, 4, 0, 10).await.unwrap()),
            vec![2, 3, 4]
        );
        assert_eq!(
            seqs(&tx.entries_after(conversation, 1, 5, 0, 2).await.unwrap()),
            vec![2, 3]
        );
        assert_eq!(
            seqs(&tx.entries_after(conversation, 5, 5, 0, 10).await.unwrap()),
            Vec::<i64>::new()
        );
        assert_eq!(
            seqs(&tx.messages_before(conversation, 1, 5, 2).await.unwrap()),
            vec![3, 4]
        );
        assert_eq!(
            seqs(&tx.messages_before(conversation, 2, 3, 10).await.unwrap()),
            vec![2]
        );
        assert_eq!(
            seqs(&tx.messages_before(conversation, 1, 1, 10).await.unwrap()),
            Vec::<i64>::new()
        );
    }

    #[tokio::test]
    async fn only_live_messages_from_others_are_counted_up_to_the_cap() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let me = UserId::new();
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        let mut entries = vec![
            entry(id, 1, ClientMsgId::new()),
            Entry {
                sender_id: me,
                ..entry(id, 2, ClientMsgId::new())
            },
            Entry {
                deleted_at: Some(start()),
                ..entry(id, 3, ClientMsgId::new())
            },
            Entry {
                kind: EntryKind::Edit,
                ..entry(id, 4, ClientMsgId::new())
            },
        ];
        entries.extend((5..=7).map(|seq| entry(id, seq, ClientMsgId::new())));
        for entry in entries {
            tx.insert_entry(entry).await.unwrap();
        }

        assert_eq!(tx.count_incoming_messages(id, me, 0, 7, 100).await, Ok(4));
        assert_eq!(tx.count_incoming_messages(id, me, 1, 4, 100).await, Ok(0));
        assert_eq!(tx.count_incoming_messages(id, me, 0, 7, 2).await, Ok(2));
        assert_eq!(tx.count_incoming_messages(id, me, 7, 7, 100).await, Ok(0));
    }

    #[tokio::test]
    async fn messages_before_skips_edits_and_deletes() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        let kinds = [
            EntryKind::Message,
            EntryKind::Edit,
            EntryKind::System,
            EntryKind::Delete,
            EntryKind::Message,
        ];
        for (seq, kind) in (1..).zip(kinds) {
            tx.insert_entry(Entry {
                kind,
                ..entry(id, seq, ClientMsgId::new())
            })
            .await
            .unwrap();
        }

        assert_eq!(
            seqs(&tx.messages_before(id, 1, 6, 2).await.unwrap()),
            vec![3, 5]
        );
        assert_eq!(
            seqs(&tx.messages_before(id, 1, 6, 10).await.unwrap()),
            vec![1, 3, 5]
        );
    }

    #[tokio::test]
    async fn positions_only_move_forward() {
        let store = InMemoryStore::new();
        let (user, device) = (UserId::new(), DeviceId::new());
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_user(user).await.unwrap();
        tx.insert_device(device, user).await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        let conversation = id;

        tx.create_cursor_if_absent(device, conversation, 3)
            .await
            .unwrap();
        tx.create_cursor_if_absent(device, conversation, 9)
            .await
            .unwrap();
        assert_eq!(tx.get_cursor(device, conversation).await.unwrap(), Some(3));

        assert!(tx.raise_cursor(device, conversation, 5).await.unwrap());
        assert!(!tx.raise_cursor(device, conversation, 4).await.unwrap());
        assert_eq!(tx.get_cursor(device, conversation).await.unwrap(), Some(5));
    }

    #[tokio::test]
    async fn revoked_devices_have_no_owner_and_are_not_listed() {
        let store = InMemoryStore::new();
        let user = UserId::new();
        let (kept, revoked) = (DeviceId::new(), DeviceId::new());
        let mut tx = store.begin().await.unwrap();
        tx.insert_user(user).await.unwrap();
        tx.insert_device(kept, user).await.unwrap();
        tx.insert_device(revoked, user).await.unwrap();
        tx.revoke_device(revoked).await.unwrap();

        assert_eq!(tx.device_owner(kept).await.unwrap(), Some(user));
        assert_eq!(tx.device_owner(revoked).await.unwrap(), None);
        assert_eq!(tx.devices_of_user(user).await.unwrap(), vec![kept]);
    }

    #[tokio::test]
    async fn rows_referring_to_missing_parents_are_rejected() {
        let store = InMemoryStore::new();
        let mut tx = store.begin().await.unwrap();
        let conversation = conversation();
        let id = conversation.id;
        tx.insert_conversation(conversation).await.unwrap();

        assert!(matches!(
            tx.insert_entry(entry(ConversationId::new(), 1, ClientMsgId::new()))
                .await,
            Err(DomainError::Store(_))
        ));
        assert!(matches!(
            tx.insert_device(DeviceId::new(), UserId::new()).await,
            Err(DomainError::Store(_))
        ));
        assert!(matches!(
            tx.put_contact(UserId::new(), UserId::new(), ContactState::RequestedOut)
                .await,
            Err(DomainError::Store(_))
        ));
        assert!(matches!(
            tx.create_cursor_if_absent(DeviceId::new(), id, 0).await,
            Err(DomainError::Store(_))
        ));
        let member = Member {
            conversation_id: id,
            user_id: UserId::new(),
            role: Role::Member,
            visible_from_seq: 1,
            left_at_seq: None,
            read_seq: 0,
        };
        assert!(matches!(
            tx.insert_member(member).await,
            Err(DomainError::Store(_))
        ));
        assert!(matches!(
            tx.raise_cursor(DeviceId::new(), id, 1).await,
            Err(DomainError::Store(_))
        ));
    }

    #[tokio::test]
    async fn a_conversation_id_cannot_be_inserted_twice() {
        let store = InMemoryStore::new();
        let first = conversation();
        let mut second = conversation();
        second.id = first.id;
        second.direct_key = Some([3; 32]);

        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(first).await.unwrap();
        assert!(matches!(
            tx.insert_conversation(second).await,
            Err(DomainError::Store(_))
        ));
        assert_eq!(tx.find_direct([3; 32]).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_message_id_cannot_be_inserted_twice() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        let first = entry(id, 1, ClientMsgId::new());
        let mut second = entry(id, 2, ClientMsgId::new());
        second.message_id = first.message_id;
        tx.insert_entry(first).await.unwrap();

        assert!(matches!(
            tx.insert_entry(second).await,
            Err(DomainError::Store(_))
        ));
        assert_eq!(
            seqs(&tx.entries_after(id, 0, 5, 0, 10).await.unwrap()),
            vec![1]
        );
    }

    #[tokio::test]
    async fn an_entry_cannot_overwrite_an_existing_seq() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        tx.insert_entry(entry(id, 1, ClientMsgId::new()))
            .await
            .unwrap();

        assert!(matches!(
            tx.insert_entry(entry(id, 1, ClientMsgId::new())).await,
            Err(DomainError::Store(_))
        ));
    }

    #[tokio::test]
    async fn raising_an_absent_position_creates_it() {
        let store = InMemoryStore::new();
        let (user, device) = (UserId::new(), DeviceId::new());
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_user(user).await.unwrap();
        tx.insert_device(device, user).await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();

        assert!(tx.raise_cursor(device, id, 4).await.unwrap());
        assert_eq!(tx.get_cursor(device, id).await.unwrap(), Some(4));
    }

    #[tokio::test]
    async fn operations_on_missing_rows_fail_as_documented() {
        let store = InMemoryStore::new();
        let mut tx = store.begin().await.unwrap();
        assert_eq!(
            tx.next_seq(ConversationId::new(), start()).await,
            Err(DomainError::ConversationNotFound)
        );
        assert_eq!(
            tx.raise_read_seq(ConversationId::new(), UserId::new(), 1)
                .await,
            Err(DomainError::NotMember)
        );
        assert_eq!(tx.revoke_device(DeviceId::new()).await, Ok(()));
    }

    #[tokio::test]
    async fn an_entry_is_found_by_message_id_only_in_its_own_conversation() {
        let store = InMemoryStore::new();
        let (home, elsewhere) = (conversation(), conversation());
        let (home_id, elsewhere_id) = (home.id, elsewhere.id);
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(home).await.unwrap();
        tx.insert_conversation(elsewhere).await.unwrap();
        let stored = entry(home_id, 1, ClientMsgId::new());
        tx.insert_entry(stored.clone()).await.unwrap();

        assert_eq!(
            tx.entry_by_message_id(home_id, stored.message_id)
                .await
                .unwrap(),
            Some(stored.clone())
        );
        assert_eq!(
            tx.entry_by_message_id(elsewhere_id, stored.message_id)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            tx.entry_by_message_id(home_id, MessageId::new())
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn update_entry_changes_only_the_mutable_fields() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        let original = entry(id, 1, ClientMsgId::new());
        tx.insert_entry(original.clone()).await.unwrap();

        let mut changed = original.clone();
        changed.body = None;
        changed.edited_at = Some(start() + TimeDelta::hours(1));
        changed.edit_seq = Some(4);
        changed.deleted_at = Some(start() + TimeDelta::hours(2));
        changed.sender_id = UserId::new();
        changed.kind = EntryKind::Edit;
        tx.update_entry(changed.clone()).await.unwrap();

        let stored = tx
            .entry_by_message_id(id, original.message_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.body, None);
        assert_eq!(stored.edited_at, changed.edited_at);
        assert_eq!(stored.edit_seq, Some(4));
        assert_eq!(stored.deleted_at, changed.deleted_at);
        assert_eq!(stored.sender_id, original.sender_id);
        assert_eq!(stored.kind, EntryKind::Message);
    }

    #[tokio::test]
    async fn updating_a_missing_entry_fails() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();

        assert!(matches!(
            tx.update_entry(entry(id, 1, ClientMsgId::new())).await,
            Err(DomainError::Store(_))
        ));
    }

    #[tokio::test]
    async fn erasing_edit_bodies_touches_only_edits_of_that_target() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        let edit = |seq, target| Entry {
            kind: EntryKind::Edit,
            target_seq: Some(target),
            ..entry(id, seq, ClientMsgId::new())
        };
        tx.insert_entry(entry(id, 1, ClientMsgId::new()))
            .await
            .unwrap();
        tx.insert_entry(entry(id, 2, ClientMsgId::new()))
            .await
            .unwrap();
        tx.insert_entry(edit(3, 1)).await.unwrap();
        tx.insert_entry(edit(4, 2)).await.unwrap();
        tx.insert_entry(edit(5, 1)).await.unwrap();

        tx.erase_edit_bodies(id, 1).await.unwrap();
        let bodies: Vec<bool> = tx
            .entries_after(id, 0, 5, 0, 10)
            .await
            .unwrap()
            .iter()
            .map(|entry| entry.body.is_some())
            .collect();
        assert_eq!(bodies, [true, true, false, true, false]);
    }

    #[tokio::test]
    async fn entries_after_skips_changes_to_messages_below_targets_from() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        for seq in 1..=3 {
            tx.insert_entry(entry(id, seq, ClientMsgId::new()))
                .await
                .unwrap();
        }
        for (seq, target) in [(4, 1), (5, 3), (6, 2)] {
            tx.insert_entry(Entry {
                kind: EntryKind::Edit,
                target_seq: Some(target),
                ..entry(id, seq, ClientMsgId::new())
            })
            .await
            .unwrap();
        }

        assert_eq!(
            seqs(&tx.entries_after(id, 2, 6, 3, 10).await.unwrap()),
            vec![3, 5]
        );
        assert_eq!(
            seqs(&tx.entries_after(id, 0, 6, 2, 3).await.unwrap()),
            vec![1, 2, 3],
            "the limit counts only returned entries"
        );
        assert_eq!(
            seqs(&tx.entries_after(id, 3, 6, 2, 2).await.unwrap()),
            vec![5, 6]
        );
    }

    #[tokio::test]
    async fn the_first_live_message_skips_changes_deleted_messages_and_the_excluded_sender() {
        let store = InMemoryStore::new();
        let conversation = conversation();
        let id = conversation.id;
        let me = UserId::new();
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(conversation).await.unwrap();
        let entries = [
            entry(id, 1, ClientMsgId::new()),
            Entry {
                sender_id: me,
                ..entry(id, 2, ClientMsgId::new())
            },
            Entry {
                deleted_at: Some(start()),
                ..entry(id, 3, ClientMsgId::new())
            },
            Entry {
                kind: EntryKind::Edit,
                ..entry(id, 4, ClientMsgId::new())
            },
            entry(id, 5, ClientMsgId::new()),
        ];
        for entry in entries {
            tx.insert_entry(entry).await.unwrap();
        }

        assert_eq!(tx.first_live_message(id, 0, 5, Some(me)).await, Ok(Some(1)));
        assert_eq!(tx.first_live_message(id, 1, 5, Some(me)).await, Ok(Some(5)));
        assert_eq!(tx.first_live_message(id, 1, 5, None).await, Ok(Some(2)));
        assert_eq!(tx.first_live_message(id, 2, 4, None).await, Ok(None));
        assert_eq!(tx.first_live_message(id, 5, 5, None).await, Ok(None));
    }

    #[tokio::test]
    async fn set_title_changes_only_the_title() {
        let store = InMemoryStore::new();
        let original = conversation();
        let id = original.id;
        let mut tx = store.begin().await.unwrap();
        tx.insert_conversation(original.clone()).await.unwrap();

        tx.set_title(id, "Trip".into()).await.unwrap();
        let stored = tx.get_conversation(id).await.unwrap().unwrap();
        assert_eq!(
            stored,
            Conversation {
                title: Some("Trip".into()),
                ..original
            }
        );
        assert_eq!(
            tx.set_title(ConversationId::new(), "Trip".into()).await,
            Err(DomainError::ConversationNotFound)
        );
    }
}

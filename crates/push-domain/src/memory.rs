//! An in-memory [`Store`] for tests.
//!
//! Each transaction holds a store-wide lock and works on a copy of the state, so
//! transactions are fully serialised and rollback is free. Never begin a second
//! transaction while one is open in the same task: it would deadlock.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::error::{DomainError, DomainResult};
use crate::ids::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
use crate::model::{ContactState, Conversation, Entry, Member};
use crate::store::{InsertOutcome, Store, Tx};

fn missing(what: &str) -> DomainError {
    DomainError::Store(format!("foreign key violation: {what} does not exist"))
}

#[derive(Debug, Clone, Default)]
struct State {
    users: HashSet<UserId>,
    devices: HashMap<DeviceId, DeviceRow>,
    contacts: HashMap<(UserId, UserId), ContactState>,
    conversations: HashMap<ConversationId, Conversation>,
    direct_index: HashMap<[u8; 32], ConversationId>,
    members: BTreeMap<(ConversationId, UserId), Member>,
    entries: BTreeMap<(ConversationId, i64), Entry>,
    client_ids: HashMap<(ConversationId, ClientMsgId), i64>,
    cursors: HashMap<(DeviceId, ConversationId), i64>,
    message_ids: HashSet<MessageId>,
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

    async fn next_seq(&mut self, id: ConversationId) -> DomainResult<i64> {
        let conversation = self
            .work
            .conversations
            .get_mut(&id)
            .ok_or(DomainError::ConversationNotFound)?;
        conversation.last_seq += 1;
        Ok(conversation.last_seq)
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
        if self.work.message_ids.contains(&entry.message_id) {
            return Err(DomainError::Store(
                "unique violation: message_id already exists".into(),
            ));
        }
        self.work.message_ids.insert(entry.message_id);
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

    async fn entries_after(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        limit: u32,
    ) -> DomainResult<Vec<Entry>> {
        if after >= up_to {
            return Ok(Vec::new());
        }
        Ok(self
            .work
            .entries
            .range((conversation, after + 1)..=(conversation, up_to))
            .take(limit as usize)
            .map(|(_, entry)| entry.clone())
            .collect())
    }

    async fn entries_before(
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
            .take(limit as usize)
            .map(|(_, entry)| entry.clone())
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
    use crate::model::{ContentType, ConversationKind, EntryKind, Role};
    use chrono::{TimeZone, Utc};

    fn conversation() -> Conversation {
        Conversation {
            id: ConversationId::new(),
            kind: ConversationKind::Direct,
            direct_key: None,
            title: None,
            created_by: UserId::new(),
            last_seq: 0,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
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
            server_ts: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            edited_at: None,
            edit_seq: None,
            deleted_at: None,
        }
    }

    fn seqs(entries: &[Entry]) -> Vec<i64> {
        entries.iter().map(|entry| entry.seq).collect()
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
        assert_eq!(tx.next_seq(id).await.unwrap(), 1);
        tx.commit().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        assert_eq!(tx.next_seq(id).await.unwrap(), 2);
        tx.rollback().await.unwrap();

        let mut tx = store.begin().await.unwrap();
        assert_eq!(tx.next_seq(id).await.unwrap(), 2);
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
            seqs(&tx.entries_after(conversation, 1, 4, 10).await.unwrap()),
            vec![2, 3, 4]
        );
        assert_eq!(
            seqs(&tx.entries_after(conversation, 1, 5, 2).await.unwrap()),
            vec![2, 3]
        );
        assert_eq!(
            seqs(&tx.entries_after(conversation, 5, 5, 10).await.unwrap()),
            Vec::<i64>::new()
        );
        assert_eq!(
            seqs(&tx.entries_before(conversation, 1, 5, 2).await.unwrap()),
            vec![3, 4]
        );
        assert_eq!(
            seqs(&tx.entries_before(conversation, 2, 3, 10).await.unwrap()),
            vec![2]
        );
        assert_eq!(
            seqs(&tx.entries_before(conversation, 1, 1, 10).await.unwrap()),
            Vec::<i64>::new()
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
            seqs(&tx.entries_after(id, 0, 5, 10).await.unwrap()),
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
            tx.next_seq(ConversationId::new()).await,
            Err(DomainError::ConversationNotFound)
        );
        assert_eq!(
            tx.raise_read_seq(ConversationId::new(), UserId::new(), 1)
                .await,
            Err(DomainError::NotMember)
        );
        assert_eq!(tx.revoke_device(DeviceId::new()).await, Ok(()));
    }
}

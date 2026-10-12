//! Shared test harness: a `Messaging` service over the in-memory store, a manual clock and
//! a recording notifier.

#![allow(dead_code)]

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use push_domain::{
    ClientMsgId, Conversation, ConversationId, DeleteMessage, DeviceId, EditText, Entry, Event,
    InMemoryStore, ManualClock, Member, MessageRef, Messaging, Policy, RecordingNotifier,
    SendReceipt, SendText, Store, SystemEvent, Tx, UserId,
};

pub type Svc = Messaging<InMemoryStore, Arc<ManualClock>, Arc<RecordingNotifier>>;

/// A verified user and one of their devices.
#[derive(Debug, Clone, Copy)]
pub struct Person {
    pub user: UserId,
    pub device: DeviceId,
}

pub struct World {
    pub svc: Svc,
    pub store: InMemoryStore,
    pub clock: Arc<ManualClock>,
    pub notifier: Arc<RecordingNotifier>,
}

impl World {
    pub fn new() -> Self {
        Self::with_policy(Policy::default())
    }

    pub fn with_policy(policy: Policy) -> Self {
        let clock = Arc::new(ManualClock::new(
            Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap(),
        ));
        let notifier = Arc::new(RecordingNotifier::default());
        let store = InMemoryStore::new();
        let svc = Messaging::with_policy(
            store.clone(),
            Arc::clone(&clock),
            Arc::clone(&notifier),
            policy,
        );
        Self {
            svc,
            store,
            clock,
            notifier,
        }
    }

    /// The delivery position of a device, read straight from the store.
    pub async fn position(&self, device: DeviceId, conversation: ConversationId) -> Option<i64> {
        let mut tx = self.store.begin().await.unwrap();
        let position = tx.get_cursor(device, conversation).await.unwrap();
        tx.rollback().await.unwrap();
        position
    }

    /// A conversation row, read straight from the store.
    pub async fn conversation(&self, id: ConversationId) -> Conversation {
        let mut tx = self.store.begin().await.unwrap();
        let conversation = tx.get_conversation(id).await.unwrap().unwrap();
        tx.rollback().await.unwrap();
        conversation
    }

    /// A member row, read straight from the store.
    pub async fn member(&self, conversation: ConversationId, person: &Person) -> Option<Member> {
        let mut tx = self.store.begin().await.unwrap();
        let member = tx.get_member(conversation, person.user).await.unwrap();
        tx.rollback().await.unwrap();
        member
    }

    /// Replaces a member row directly in the store, to set up visibility ranges that the
    /// Plan 1 API cannot create yet.
    pub async fn set_visibility(
        &self,
        conversation: ConversationId,
        user: UserId,
        visible_from_seq: i64,
        left_at_seq: Option<i64>,
    ) {
        let mut tx = self.store.begin().await.unwrap();
        let mut member = tx.get_member(conversation, user).await.unwrap().unwrap();
        member.visible_from_seq = visible_from_seq;
        member.left_at_seq = left_at_seq;
        tx.insert_member(member).await.unwrap();
        tx.commit().await.unwrap();
    }

    /// Registers a verified user with one device.
    pub async fn person(&self) -> Person {
        let person = Person {
            user: UserId::new(),
            device: DeviceId::new(),
        };
        self.svc.register_user(person.user).await.unwrap();
        self.svc
            .register_device(person.device, person.user)
            .await
            .unwrap();
        person
    }

    /// Registers another device for an existing person.
    pub async fn add_device(&self, person: &Person) -> DeviceId {
        let device = DeviceId::new();
        self.svc.register_device(device, person.user).await.unwrap();
        device
    }

    /// Makes two people accepted contacts.
    pub async fn befriend(&self, a: &Person, b: &Person) {
        self.svc.contact_request(a.user, b.user).await.unwrap();
        self.svc.contact_accept(b.user, a.user).await.unwrap();
    }

    /// Makes two people contacts and opens their direct conversation.
    pub async fn direct_chat(&self, a: &Person, b: &Person) -> ConversationId {
        self.befriend(a, b).await;
        self.svc.open_direct(a.user, b.user).await.unwrap()
    }

    /// Makes `admin` a contact of every other member and creates a group titled "Group".
    pub async fn group(&self, admin: &Person, members: &[&Person]) -> ConversationId {
        for member in members {
            self.befriend(admin, member).await;
        }
        let ids: Vec<UserId> = members.iter().map(|member| member.user).collect();
        self.svc
            .create_group(admin.user, "Group", &ids)
            .await
            .unwrap()
    }

    /// Sends a text message and expects it to succeed.
    pub async fn send(
        &self,
        from: &Person,
        conversation: ConversationId,
        body: &str,
    ) -> SendReceipt {
        self.svc
            .send_text(text(from, conversation, body))
            .await
            .unwrap()
    }

    /// Edits a message by its server id and expects it to succeed.
    pub async fn edit(
        &self,
        from: &Person,
        conversation: ConversationId,
        target: &SendReceipt,
        body: &str,
    ) -> SendReceipt {
        self.svc
            .edit_text(edit(
                from,
                conversation,
                MessageRef::Id(target.message_id),
                body,
            ))
            .await
            .unwrap()
    }
}

/// A delete command with a fresh client message id.
pub fn delete(from: &Person, conversation: ConversationId, target: MessageRef) -> DeleteMessage {
    DeleteMessage {
        sender: from.user,
        device: from.device,
        conversation,
        client_msg_id: ClientMsgId::new(),
        target,
    }
}

/// An edit command with a fresh client message id.
pub fn edit(
    from: &Person,
    conversation: ConversationId,
    target: MessageRef,
    body: &str,
) -> EditText {
    EditText {
        sender: from.user,
        device: from.device,
        conversation,
        client_msg_id: ClientMsgId::new(),
        target,
        body: body.as_bytes().to_vec(),
        client_ts: None,
    }
}

/// A send command with a fresh client message id.
pub fn text(from: &Person, conversation: ConversationId, body: &str) -> SendText {
    SendText {
        sender: from.user,
        device: from.device,
        conversation,
        client_msg_id: ClientMsgId::new(),
        body: body.as_bytes().to_vec(),
        client_ts: None,
    }
}

pub fn bodies(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| String::from_utf8(entry.body.clone().unwrap_or_default()).unwrap())
        .collect()
}

pub fn seqs(entries: &[Entry]) -> Vec<i64> {
    entries.iter().map(|entry| entry.seq).collect()
}

/// The system events recorded by these entries; `None` for other entries.
pub fn system_events(entries: &[Entry]) -> Vec<Option<SystemEvent>> {
    entries
        .iter()
        .map(|entry| SystemEvent::decode(entry.body.as_deref().unwrap_or_default()))
        .collect()
}

/// The event that tells these people to sync.
pub fn entries_for(people: &[&Person]) -> Event {
    let mut users: Vec<UserId> = people.iter().map(|person| person.user).collect();
    users.sort();
    Event::EntriesAvailable { users }
}

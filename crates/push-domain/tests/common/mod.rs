//! Shared test harness: a `Messaging` service over the in-memory store, a manual clock and
//! a recording notifier.

#![allow(dead_code)]

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use push_domain::{
    ClientMsgId, ConversationId, DeviceId, Entry, Event, InMemoryStore, ManualClock, Messaging,
    RecordingNotifier, SendReceipt, SendText, Store, Tx, UserId,
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
        let clock = Arc::new(ManualClock::new(
            Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap(),
        ));
        let notifier = Arc::new(RecordingNotifier::default());
        let store = InMemoryStore::new();
        let svc = Messaging::new(store.clone(), Arc::clone(&clock), Arc::clone(&notifier));
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

/// The event that tells these people to sync.
pub fn entries_for(people: &[&Person]) -> Event {
    let mut users: Vec<UserId> = people.iter().map(|person| person.user).collect();
    users.sort();
    Event::EntriesAvailable { users }
}

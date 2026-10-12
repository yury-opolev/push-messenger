mod common;

use chrono::{TimeDelta, TimeZone, Utc};
use common::{Person, World, entries_for, text};
use push_domain::{
    Clock, ConversationId, DeviceId, DomainError, Event, InMemoryStore, ManualClock, Messaging,
    Notifier, SendText, Store, Tx, UserId, ValidationError,
};
use std::sync::Arc;

/// Proves events are published after commit: it begins its own transaction (which would
/// wait forever on the in-memory store's lock if the sender's were still open) and checks
/// the new message is already visible.
struct CommitProbe {
    store: InMemoryStore,
    observed: std::sync::Mutex<Vec<i64>>,
}

#[async_trait::async_trait]
impl Notifier for CommitProbe {
    async fn publish(&self, events: &[Event]) {
        let Some(Event::EntriesAvailable { users }) = events.first() else {
            return;
        };
        let mut tx = tokio::time::timeout(std::time::Duration::from_secs(2), self.store.begin())
            .await
            .expect("published while the sending transaction was still open")
            .unwrap();
        // The conversation's last_seq as seen by an independent transaction.
        let memberships = tx.memberships(users[0]).await.unwrap();
        let mut observed = self.observed.lock().unwrap();
        observed.extend(
            memberships
                .iter()
                .map(|(conversation, _)| conversation.last_seq),
        );
    }
}

async fn register(svc: &Messaging<InMemoryStore, Arc<ManualClock>, Arc<CommitProbe>>) -> Person {
    let person = Person {
        user: UserId::new(),
        device: DeviceId::new(),
    };
    svc.register_user(person.user).await.unwrap();
    svc.register_device(person.device, person.user)
        .await
        .unwrap();
    person
}

#[tokio::test]
async fn events_are_published_only_after_the_send_commits() {
    let store = InMemoryStore::new();
    let probe = Arc::new(CommitProbe {
        store: store.clone(),
        observed: std::sync::Mutex::new(Vec::new()),
    });
    let clock = Arc::new(ManualClock::new(
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap(),
    ));
    let svc = Messaging::new(store, clock, Arc::clone(&probe));
    let alice = register(&svc).await;
    let bob = register(&svc).await;
    svc.contact_request(alice.user, bob.user).await.unwrap();
    svc.contact_accept(bob.user, alice.user).await.unwrap();
    let chat = svc.open_direct(alice.user, bob.user).await.unwrap();

    svc.send_text(text(&alice, chat, "hi")).await.unwrap();
    assert_eq!(*probe.observed.lock().unwrap(), [1]);
}

#[tokio::test]
async fn messages_get_consecutive_seq_numbers_starting_at_one() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    let first = world.send(&alice, chat, "one").await;
    let second = world.send(&bob, chat, "two").await;
    let third = world.send(&alice, chat, "three").await;
    assert_eq!([first.seq, second.seq, third.seq], [1, 2, 3]);
}

#[tokio::test]
async fn a_retry_returns_the_original_and_leaves_no_gap() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    let command = text(&alice, chat, "hello");
    let first = world.svc.send_text(command.clone()).await.unwrap();
    let retry = world.svc.send_text(command).await.unwrap();
    assert_eq!(retry, first);

    assert_eq!(world.send(&bob, chat, "hi").await.seq, 2);
}

#[tokio::test]
async fn another_sender_cannot_reuse_a_client_msg_id() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    let command = text(&alice, chat, "hello");
    world.svc.send_text(command.clone()).await.unwrap();
    let forged = SendText {
        sender: bob.user,
        device: bob.device,
        ..command
    };
    assert_eq!(
        world.svc.send_text(forged).await,
        Err(DomainError::ClientIdConflict)
    );
}

#[tokio::test]
async fn an_invalid_message_is_rejected_and_uses_no_seq() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    assert_eq!(
        world.svc.send_text(text(&alice, chat, "")).await,
        Err(DomainError::Invalid(ValidationError::EmptyBody))
    );
    assert_eq!(world.send(&alice, chat, "ok").await.seq, 1);
}

#[tokio::test]
async fn only_members_can_send() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let chat = world.direct_chat(&alice, &bob).await;
    world.befriend(&alice, &carol).await;

    assert_eq!(
        world.svc.send_text(text(&carol, chat, "hi")).await,
        Err(DomainError::NotMember)
    );
    assert_eq!(world.send(&alice, chat, "still gapless").await.seq, 1);
}

#[tokio::test]
async fn a_blocked_contact_cannot_send() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.svc.contact_block(bob.user, alice.user).await.unwrap();

    assert_eq!(
        world.svc.send_text(text(&alice, chat, "hi")).await,
        Err(DomainError::NotContact)
    );

    world
        .svc
        .contact_unblock(bob.user, alice.user)
        .await
        .unwrap();
    world.befriend(&alice, &bob).await;
    assert_eq!(world.send(&alice, chat, "after").await.seq, 1);
}

#[tokio::test]
async fn a_retry_after_being_blocked_still_returns_the_original() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    let command = text(&alice, chat, "hello");
    let first = world.svc.send_text(command.clone()).await.unwrap();
    world.svc.contact_block(bob.user, alice.user).await.unwrap();

    assert_eq!(world.svc.send_text(command).await, Ok(first));

    world
        .svc
        .contact_unblock(bob.user, alice.user)
        .await
        .unwrap();
    world.befriend(&alice, &bob).await;
    assert_eq!(world.send(&alice, chat, "again").await.seq, 2);
}

#[tokio::test]
async fn a_retry_publishes_nothing() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let command = text(&alice, chat, "hello");
    world.svc.send_text(command.clone()).await.unwrap();
    world.notifier.take();

    world.svc.send_text(command).await.unwrap();
    assert!(world.notifier.take().is_empty());
}

#[tokio::test]
async fn the_device_must_belong_to_the_sender() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    let command = SendText {
        device: bob.device,
        ..text(&alice, chat, "hi")
    };
    assert_eq!(
        world.svc.send_text(command).await,
        Err(DomainError::UnknownDevice)
    );
}

#[tokio::test]
async fn sending_to_an_unknown_conversation_fails() {
    let world = World::new();
    let alice = world.person().await;
    assert_eq!(
        world
            .svc
            .send_text(text(&alice, ConversationId::new(), "hi"))
            .await,
        Err(DomainError::ConversationNotFound)
    );
}

#[tokio::test]
async fn a_send_tells_every_member_to_sync_and_a_failed_one_tells_nobody() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.notifier.take();

    world.send(&alice, chat, "hi").await;
    assert_eq!(world.notifier.take(), [entries_for(&[&alice, &bob])]);

    let _ = world.svc.send_text(text(&alice, chat, "")).await;
    assert!(world.notifier.take().is_empty());
}

#[tokio::test]
async fn the_server_timestamp_comes_from_the_clock() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let at = Utc.with_ymd_and_hms(2026, 3, 4, 5, 6, 7).unwrap();
    world.clock.set(at);

    assert_eq!(world.send(&alice, chat, "hi").await.server_ts, at);
}

#[tokio::test]
async fn opening_and_sending_set_the_conversations_updated_at() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let opened_at = world.clock.now();
    let chat = world.direct_chat(&alice, &bob).await;
    assert_eq!(world.conversation(chat).await.updated_at, opened_at);

    world.clock.advance(TimeDelta::minutes(5));
    world.send(&alice, chat, "hi").await;
    assert_eq!(
        world.conversation(chat).await.updated_at,
        opened_at + TimeDelta::minutes(5)
    );
}

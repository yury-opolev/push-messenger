mod common;

use chrono::TimeDelta;
use common::{Person, World, delete, edit, entries_for, seqs};
use push_domain::{
    Clock, ConversationId, DomainError, Entry, EntryKind, MessageRef, Policy, SendReceipt,
};

/// Alice and bob in a direct chat, with one message "hello" from alice at seq 1.
async fn chat_with_one_message() -> (World, Person, Person, ConversationId, SendReceipt) {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let hello = world.send(&alice, chat, "hello").await;
    (world, alice, bob, chat, hello)
}

fn by_id(target: &SendReceipt) -> MessageRef {
    MessageRef::Id(target.message_id)
}

async fn synced(world: &World, person: &Person) -> Vec<Entry> {
    world.svc.sync(person.device, 100).await.unwrap().entries
}

#[tokio::test]
async fn a_delete_erases_the_message_and_every_edit_of_it() {
    let (world, alice, bob, chat, hello) = chat_with_one_message().await;
    world.edit(&alice, chat, &hello, "v1").await;
    world.edit(&alice, chat, &hello, "v2").await;
    world.clock.advance(TimeDelta::minutes(1));

    let receipt = world
        .svc
        .delete_message(delete(&alice, chat, by_id(&hello)))
        .await
        .unwrap();
    assert_eq!(receipt.seq, 4);

    let entries = synced(&world, &bob).await;
    assert_eq!(seqs(&entries), [1, 2, 3, 4]);
    assert!(entries.iter().all(|entry| entry.body.is_none()));
    assert_eq!(entries[0].deleted_at, Some(world.clock.now()));
    assert_eq!(entries[3].kind, EntryKind::Delete);
    assert_eq!(entries[3].target_seq, Some(1));
}

#[tokio::test]
async fn a_delete_retry_returns_the_original_and_changes_nothing() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    let command = delete(&alice, chat, by_id(&hello));
    let first = world.svc.delete_message(command.clone()).await.unwrap();
    world.notifier.take();

    assert_eq!(world.svc.delete_message(command).await, Ok(first));
    assert!(world.notifier.take().is_empty());
    assert_eq!(world.send(&alice, chat, "next").await.seq, 3);
}

#[tokio::test]
async fn a_second_delete_of_the_same_message_is_refused() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    world
        .svc
        .delete_message(delete(&alice, chat, by_id(&hello)))
        .await
        .unwrap();

    assert_eq!(
        world
            .svc
            .delete_message(delete(&alice, chat, by_id(&hello)))
            .await,
        Err(DomainError::AlreadyDeleted)
    );
}

#[tokio::test]
async fn a_deleted_message_cannot_be_edited() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    world
        .svc
        .delete_message(delete(&alice, chat, by_id(&hello)))
        .await
        .unwrap();

    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), "back"))
            .await,
        Err(DomainError::AlreadyDeleted)
    );
}

#[tokio::test]
async fn only_the_sender_can_delete() {
    let (world, _alice, bob, chat, hello) = chat_with_one_message().await;
    assert_eq!(
        world
            .svc
            .delete_message(delete(&bob, chat, by_id(&hello)))
            .await,
        Err(DomainError::NotSender)
    );
}

#[tokio::test]
async fn a_delete_is_allowed_until_the_window_closes() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    let second = world.send(&alice, chat, "second").await;
    world.clock.advance(TimeDelta::hours(48));
    world
        .svc
        .delete_message(delete(&alice, chat, by_id(&hello)))
        .await
        .unwrap();

    world.clock.advance(TimeDelta::seconds(1));
    assert_eq!(
        world
            .svc
            .delete_message(delete(&alice, chat, by_id(&second)))
            .await,
        Err(DomainError::WindowExpired)
    );
}

#[tokio::test]
async fn the_delete_window_is_separate_from_the_edit_window() {
    let world = World::with_policy(Policy {
        edit_window: TimeDelta::minutes(1),
        ..Policy::default()
    });
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let hello = world.send(&alice, chat, "hello").await;
    world.clock.advance(TimeDelta::minutes(2));

    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), "late"))
            .await,
        Err(DomainError::WindowExpired)
    );
    assert!(
        world
            .svc
            .delete_message(delete(&alice, chat, by_id(&hello)))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_delete_tells_every_member_to_sync() {
    let (world, alice, bob, chat, hello) = chat_with_one_message().await;
    world.notifier.take();

    world
        .svc
        .delete_message(delete(&alice, chat, by_id(&hello)))
        .await
        .unwrap();
    assert_eq!(world.notifier.take(), [entries_for(&[&alice, &bob])]);
}

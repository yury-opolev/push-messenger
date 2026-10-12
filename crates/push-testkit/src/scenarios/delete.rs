use crate::{Backend, Case, Person, World, case_list, delete, edit, entries_for, seqs};
use chrono::TimeDelta;
use push_domain::{
    Clock, ConversationId, DomainError, Entry, EntryKind, MessageRef, Policy, SendReceipt, Store,
};

/// Alice and bob in a direct chat, with one message "hello" from alice at seq 1.
async fn chat_with_one_message<B: Backend>(
    backend: &B,
) -> (World<B::Store>, Person, Person, ConversationId, SendReceipt) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let hello = world.send(&alice, chat, "hello").await;
    (world, alice, bob, chat, hello)
}

fn by_id(target: &SendReceipt) -> MessageRef {
    MessageRef::Id(target.message_id)
}

async fn synced<S: Store>(world: &World<S>, person: &Person) -> Vec<Entry> {
    world.svc.sync(person.device, 100).await.unwrap().entries
}

async fn a_delete_erases_the_message_and_every_edit_of_it<B: Backend>(backend: &B) {
    let (world, alice, bob, chat, hello) = chat_with_one_message(backend).await;
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

async fn a_delete_retry_returns_the_original_and_changes_nothing<B: Backend>(backend: &B) {
    let (world, alice, _bob, chat, hello) = chat_with_one_message(backend).await;
    let command = delete(&alice, chat, by_id(&hello));
    let first = world.svc.delete_message(command.clone()).await.unwrap();
    world.notifier.take();

    assert_eq!(world.svc.delete_message(command).await, Ok(first));
    assert!(world.notifier.take().is_empty());
    assert_eq!(world.send(&alice, chat, "next").await.seq, 3);
}

async fn a_second_delete_of_the_same_message_is_refused<B: Backend>(backend: &B) {
    let (world, alice, _bob, chat, hello) = chat_with_one_message(backend).await;
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

async fn a_deleted_message_cannot_be_edited<B: Backend>(backend: &B) {
    let (world, alice, _bob, chat, hello) = chat_with_one_message(backend).await;
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

async fn only_the_sender_can_delete<B: Backend>(backend: &B) {
    let (world, _alice, bob, chat, hello) = chat_with_one_message(backend).await;
    assert_eq!(
        world
            .svc
            .delete_message(delete(&bob, chat, by_id(&hello)))
            .await,
        Err(DomainError::NotSender)
    );
}

async fn a_delete_is_allowed_until_the_window_closes<B: Backend>(backend: &B) {
    let (world, alice, _bob, chat, hello) = chat_with_one_message(backend).await;
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

async fn the_delete_window_is_separate_from_the_edit_window<B: Backend>(backend: &B) {
    let world = World::with_policy(
        backend,
        Policy {
            edit_window: TimeDelta::minutes(1),
            ..Policy::default()
        },
    )
    .await;
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

async fn a_delete_tells_every_member_to_sync<B: Backend>(backend: &B) {
    let (world, alice, bob, chat, hello) = chat_with_one_message(backend).await;
    world.notifier.take();

    world
        .svc
        .delete_message(delete(&alice, chat, by_id(&hello)))
        .await
        .unwrap();
    assert_eq!(world.notifier.take(), [entries_for(&[&alice, &bob])]);
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        a_delete_erases_the_message_and_every_edit_of_it,
        a_delete_retry_returns_the_original_and_changes_nothing,
        a_second_delete_of_the_same_message_is_refused,
        a_deleted_message_cannot_be_edited,
        only_the_sender_can_delete,
        a_delete_is_allowed_until_the_window_closes,
        the_delete_window_is_separate_from_the_edit_window,
        a_delete_tells_every_member_to_sync,
    ]
}

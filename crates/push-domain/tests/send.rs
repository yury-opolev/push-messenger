mod common;

use chrono::{TimeZone, Utc};
use common::{World, text};
use push_domain::{ConversationId, DomainError, SendText, ValidationError};

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
}

#[tokio::test]
async fn a_retry_is_not_notified_again() {
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
async fn members_are_notified_only_after_a_successful_send() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.notifier.take();

    world.send(&alice, chat, "hi").await;
    let calls = world.notifier.take();
    assert_eq!(calls.len(), 1);
    let mut notified = calls[0].clone();
    notified.sort();
    let mut expected = vec![alice.user, bob.user];
    expected.sort();
    assert_eq!(notified, expected);

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

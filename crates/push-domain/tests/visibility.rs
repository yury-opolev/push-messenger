mod common;

use common::{Person, World, seqs};
use push_domain::{ConversationId, DomainError};

/// A chat between alice and bob with six messages from alice. Bob's visibility is set
/// before the messages are sent, because a delivery position cannot be lowered.
async fn chat_with_six_messages(
    visible_from_seq: i64,
    left_at_seq: Option<i64>,
) -> (World, Person, ConversationId) {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world
        .set_visibility(chat, bob.user, visible_from_seq, left_at_seq)
        .await;
    for _ in 0..6 {
        world.send(&alice, chat, "x").await;
    }
    (world, bob, chat)
}

#[tokio::test]
async fn sync_starts_at_visible_from() {
    let (world, bob, _chat) = chat_with_six_messages(3, None).await;
    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [3, 4, 5, 6]);
}

#[tokio::test]
async fn sync_and_history_stop_at_left_at() {
    let (world, bob, chat) = chat_with_six_messages(1, Some(4)).await;
    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [1, 2, 3, 4]);
    let history = world.svc.history(bob.user, chat, None, 10).await.unwrap();
    assert_eq!(seqs(&history), [1, 2, 3, 4]);
}

#[tokio::test]
async fn history_does_not_go_below_visible_from() {
    let (world, bob, chat) = chat_with_six_messages(3, None).await;
    let history = world.svc.history(bob.user, chat, None, 10).await.unwrap();
    assert_eq!(seqs(&history), [3, 4, 5, 6]);
}

#[tokio::test]
async fn ack_and_read_beyond_left_at_are_rejected() {
    let (world, bob, chat) = chat_with_six_messages(1, Some(4)).await;
    assert_eq!(
        world.svc.ack(bob.device, chat, 5).await,
        Err(DomainError::SeqOutOfRange)
    );
    assert_eq!(
        world.svc.read_up_to(bob.user, chat, 5).await,
        Err(DomainError::SeqOutOfRange)
    );
    assert_eq!(world.svc.ack(bob.device, chat, 4).await, Ok(true));
}

#[tokio::test]
async fn an_ack_below_the_floor_changes_nothing() {
    let (world, bob, chat) = chat_with_six_messages(3, None).await;
    let before = world.position(bob.device, chat).await;

    assert_eq!(world.svc.ack(bob.device, chat, 1).await, Ok(false));
    assert_eq!(world.position(bob.device, chat).await, before);
}
